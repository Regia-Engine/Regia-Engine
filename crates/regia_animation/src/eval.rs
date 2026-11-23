// SPDX-License-Identifier: MPL-2.0
//! Graph evaluation: the runtime instance tree and per-frame evaluation of every node type.
//!
//! Specification: `docs/specs/17-animation.md` §§ 2–3. Evaluation samples and blends into a
//! compact local-space [`Pose`], applies IK post-processes, and reports notifies, state
//! events, morph weights, curve values and root motion through `EvalOutput`-style fields
//! on the [`EvalContext`].

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;

use crate::clip::{AnimationClip, ClipStep, Notify, RootMotionMode, filter_root_motion};
use crate::graph::{AnimGraph, AnimNode, IkTarget, LayerMode};
use crate::ik::{IkChain, LookAtSettings, solve_look_at_with_globals, solve_two_bone_with_globals};
use crate::inertialize::{Inertializer, Switch, WeightInertia};
use crate::montage_player::SlotLayers;
use crate::num::unit_weight;
use crate::params::ParameterSet;
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::state_machine::{Condition, StateMachineDef, TransitionBlend};
use crate::sync::{SyncGroups, SyncMember, SyncPlay, SyncQuery, SyncSetup};

/// Errors raised during graph evaluation.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EvalError {
    /// The graph references a clip that is not in the animator's clip library.
    #[error("clip {0:?} is not loaded")]
    MissingClip(String),
    /// A joint referenced by name does not exist in the rig's skeleton.
    #[error("joint {0:?} not found in skeleton")]
    MissingJoint(String),
}

/// Playback state of one clip instance.
#[derive(Clone, Debug, Default)]
pub struct ClipState {
    /// Current clip-local time.
    pub time: f32,
    /// True when a non-looping clip has reached its end (its start, playing in reverse).
    pub finished: bool,
    /// Whether the clip has been evaluated since it (re)started; the first evaluation also
    /// fires notifies sitting exactly at the start time.
    pub started: bool,
}

/// Root motion of one evaluated sub-graph (already filtered by its extraction mode). Graph
/// nodes combine their children's deltas the way they combine their poses (ADR-871).
#[derive(Clone, Copy, Debug, PartialEq)]
struct RootDelta {
    translation: Vec3,
    rotation: Quat,
}

impl RootDelta {
    const ZERO: Self = RootDelta {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
    };

    /// `self·(1−t) + other·t` (rotation by normalised lerp on the shortest arc).
    fn lerp(self, other: Self, t: f32) -> Self {
        if t <= 0.0 {
            return self;
        }
        if t >= 1.0 {
            return other;
        }
        RootDelta {
            translation: self.translation.lerp(other.translation, t),
            rotation: self.rotation.lerp(other.rotation, t).normalize(),
        }
    }

    /// `self` followed by `w` of `other` (additive application).
    fn add_scaled(self, other: Self, w: f32) -> Self {
        if w <= 0.0 {
            return self;
        }
        RootDelta {
            translation: self.translation + other.translation * w,
            rotation: (self.rotation * Quat::IDENTITY.lerp(other.rotation, w.min(1.0))).normalize(),
        }
    }
}

/// Morph weights one evaluated sub-graph produces, sorted by key: tracks for every morph mesh
/// by target, per-node tracks by `(node, target)`. Nodes combine them like poses (ADR-871): a
/// key one side of a blend lacks reads as 0 (a morph target's rest weight), and every key
/// either side has is kept, so a weight fades out instead of holding and then popping.
#[derive(Clone, Debug, Default, PartialEq)]
struct MorphMix {
    shared: Vec<(u32, f32)>,
    nodes: Vec<((u32, u32), f32)>,
}

/// Most morph lists of each kind [`MorphPools`] keeps between frames.
const MAX_POOLED_MORPH_LISTS: usize = 64;

/// Reused morph-list buffers. Evaluation returns them through [`FinishedEval`] so the next
/// frame does not allocate a new list per blend.
#[derive(Default)]
struct MorphPools {
    shared: Vec<Vec<(u32, f32)>>,
    nodes: Vec<Vec<((u32, u32), f32)>>,
}

impl MorphPools {
    fn take_shared(&mut self) -> Vec<(u32, f32)> {
        self.shared.pop().unwrap_or_default()
    }

    fn take_nodes(&mut self) -> Vec<((u32, u32), f32)> {
        self.nodes.pop().unwrap_or_default()
    }

    // A list without an allocation has nothing to reuse, and a blend of two morph-less inputs
    // returns one that never came from the pool: pooling it grew the pool by one entry per
    // blend per frame, forever (audit corr-animation). The pool length is also capped.
    fn give_shared(&mut self, mut values: Vec<(u32, f32)>) {
        if values.capacity() == 0 || self.shared.len() >= MAX_POOLED_MORPH_LISTS {
            return;
        }
        values.clear();
        self.shared.push(values);
    }

    fn give_nodes(&mut self, mut values: Vec<((u32, u32), f32)>) {
        if values.capacity() == 0 || self.nodes.len() >= MAX_POOLED_MORPH_LISTS {
            return;
        }
        values.clear();
        self.nodes.push(values);
    }

    fn recycle(&mut self, mix: MorphMix) {
        self.give_shared(mix.shared);
        self.give_nodes(mix.nodes);
    }
}

/// Union of two key-sorted lists into `out` (cleared, capacity kept).
///
/// Each key is combined with `f(left, right)`. A missing side is `None`.
fn merge_sorted_into<K: Ord + Copy>(
    a: &[(K, f32)],
    b: &[(K, f32)],
    f: &impl Fn(Option<f32>, Option<f32>) -> f32,
    out: &mut Vec<(K, f32)>,
) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    loop {
        match (a.get(i), b.get(j)) {
            (Some(&(ka, va)), Some(&(kb, vb))) => match ka.cmp(&kb) {
                std::cmp::Ordering::Less => {
                    out.push((ka, f(Some(va), None)));
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push((kb, f(None, Some(vb))));
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push((ka, f(Some(va), Some(vb))));
                    i += 1;
                    j += 1;
                }
            },
            (Some(&(ka, va)), None) => {
                out.push((ka, f(Some(va), None)));
                i += 1;
            }
            (None, Some(&(kb, vb))) => {
                out.push((kb, f(None, Some(vb))));
                j += 1;
            }
            (None, None) => break,
        }
    }
}

impl MorphMix {
    fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.nodes.is_empty()
    }

    fn combine(
        &self,
        other: &Self,
        f: impl Fn(Option<f32>, Option<f32>) -> f32,
        pools: &mut MorphPools,
    ) -> Self {
        if self.is_empty() && other.is_empty() {
            return MorphMix::default();
        }
        let mut shared = pools.take_shared();
        let mut nodes = pools.take_nodes();
        merge_sorted_into(&self.shared, &other.shared, &f, &mut shared);
        merge_sorted_into(&self.nodes, &other.nodes, &f, &mut nodes);
        MorphMix { shared, nodes }
    }

    /// `self·(1−t) + other·t`, missing keys as 0.
    fn lerp(&self, other: &Self, t: f32, pools: &mut MorphPools) -> Self {
        self.combine(
            other,
            |a, b| {
                let (a, b) = (a.unwrap_or(0.0), b.unwrap_or(0.0));
                a + (b - a) * t
            },
            pools,
        )
    }

    /// An override layer: keys the layer has move towards it by `w`; the others keep `self`.
    fn override_with(&self, layer: &Self, w: f32, pools: &mut MorphPools) -> Self {
        self.combine(
            layer,
            |a, b| {
                let a = a.unwrap_or(0.0);
                b.map_or(a, |b| a + (b - a) * w)
            },
            pools,
        )
    }

    /// An additive layer: `self + w · add`.
    fn add_scaled(&self, add: &Self, w: f32, pools: &mut MorphPools) -> Self {
        self.combine(add, |a, b| a.unwrap_or(0.0) + b.unwrap_or(0.0) * w, pools)
    }
}

/// What evaluating one node produces besides its pose: root motion and morph weights.
#[derive(Clone, Debug, PartialEq)]
struct NodeOut {
    root: RootDelta,
    morphs: MorphMix,
}

impl NodeOut {
    const ZERO: NodeOut = NodeOut {
        root: RootDelta::ZERO,
        morphs: MorphMix {
            shared: Vec::new(),
            nodes: Vec::new(),
        },
    };

    fn lerp(self, other: Self, t: f32, pools: &mut MorphPools) -> Self {
        let root = self.root.lerp(other.root, t);
        let morphs = self.morphs.lerp(&other.morphs, t, pools);
        pools.recycle(self.morphs);
        pools.recycle(other.morphs);
        NodeOut { root, morphs }
    }

    fn add_scaled(self, other: Self, w: f32, pools: &mut MorphPools) -> Self {
        let root = self.root.add_scaled(other.root, w);
        let morphs = self.morphs.add_scaled(&other.morphs, w, pools);
        pools.recycle(self.morphs);
        pools.recycle(other.morphs);
        NodeOut { root, morphs }
    }
}

/// How one clip is played by [`eval_clip`].
#[derive(Clone, Copy, Debug)]
struct ClipPlay {
    looping: bool,
    speed: f32,
    /// A shared normalised step (`0..=1` clock) that replaces the clip's own advance.
    sync: Option<ClipStep>,
    /// Whether the clip's notifies fire (followers of a synced blend space stay quiet).
    notifies: bool,
}

impl ClipPlay {
    fn own(looping: bool, speed: f32) -> Self {
        ClipPlay {
            looping,
            speed,
            sync: None,
            notifies: true,
        }
    }
}

/// How much a sub-graph counts this frame. `pose` weights its pose, root motion, morphs and
/// curves; `audible` weights its notifies. They are equal except under a cross-fade's fire
/// frame, where the entered state adds nothing to the pose yet but its start notifies are
/// heard. Nodes scale both with `*`, so a branch at weight 0 stays silent all the way down.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Weight {
    pose: f32,
    audible: f32,
}

impl Weight {
    const FULL: Weight = Weight {
        pose: 1.0,
        audible: 1.0,
    };
    const ZERO: Weight = Weight {
        pose: 0.0,
        audible: 0.0,
    };

    /// The same audibility with no pose contribution.
    fn silent_pose(self) -> Weight {
        Weight {
            pose: 0.0,
            audible: self.audible,
        }
    }
}

impl std::ops::Mul<f32> for Weight {
    type Output = Weight;

    fn mul(self, factor: f32) -> Weight {
        Weight {
            pose: self.pose * factor,
            audible: self.audible * factor,
        }
    }
}

/// A state-machine transition in progress.
#[derive(Clone, Debug)]
pub struct ActiveTransition {
    /// Index into the machine's transition table.
    pub def: usize,
    /// State the transition blends from.
    pub from: usize,
    /// Seconds since the transition started.
    pub elapsed: f32,
    /// Cross-fade (the source state is evaluated and blended) or inertialized (only the
    /// target is evaluated; the offset lives in [`StateMachineInstance::inertia`]).
    pub blend: TransitionBlend,
}

/// Runtime state of one state machine.
#[derive(Clone, Debug)]
pub struct StateMachineInstance {
    /// Per-state graph instances (parallel to the definition's states).
    pub states: Vec<GraphInstance>,
    /// Current state index.
    pub current: usize,
    /// Transition in progress, if any.
    pub transition: Option<ActiveTransition>,
    /// Whether the initial state's enter event has been emitted.
    pub entered_initial: bool,
    /// The decaying pose offset of the latest inertialized transition. It outlives the
    /// transition record when a cross-fade interrupts it, and keeps decaying on top.
    pub inertia: Inertializer,
    /// Whether this machine keeps its previous output (for inertialization velocities); set
    /// when its definition has an inertialized transition.
    pub tracks_history: bool,
    /// The machine's output last frame (valid when `history_valid`).
    pub prev_output: Pose,
    /// Whether `prev_output` holds a real previous frame.
    pub history_valid: bool,
}

/// Playback state of one blend-space node, plus the scratch its evaluation reuses.
///
/// `states` is one [`ClipState`] per sample, then one more that holds a synced space's
/// shared normalised clock (`time` is the phase in `0..1`). The other fields are runtime
/// cache. A pose snapshot stores the clocks only ([`crate::pose_eval`]).
///
/// Replaces the `Vec<ClipState>` payload of `GraphInstance::BlendSpace1D` and
/// `GraphInstance::BlendSpace2D`. A match that took that `Vec` now reads [`BlendRuntime::states`].
#[derive(Clone, Debug)]
pub struct BlendRuntime {
    /// Clip clocks. The last entry is the shared clock when `from_node` built the runtime.
    pub states: Vec<ClipState>,
    /// Sorted finite-sample indices for a 1D space.
    order: Vec<usize>,
    /// Active-sample weights from the latest evaluation.
    weights: Vec<(usize, f32)>,
    /// Weights above the blend epsilon, in sample order.
    active: Vec<(usize, f32)>,
    /// On-the-fly triangulation when the 2D asset has none.
    triangles: Vec<[usize; 3]>,
    /// Sample count `triangles` was built for. `usize::MAX` until the first build.
    triangle_samples: usize,
    /// Fingerprint of the sample points `triangles` was built for: an in-place move does not
    /// change the count, and must still rebuild the triangulation.
    triangle_points_hash: u64,
    /// Clips resolved for the latest evaluation.
    clips: Vec<Arc<AnimationClip>>,
}

impl BlendRuntime {
    /// `count` default clocks (samples plus the shared clock) and empty scratch.
    #[must_use]
    pub fn from_count(count: usize) -> Self {
        Self {
            states: vec![ClipState::default(); count],
            order: Vec::new(),
            weights: Vec::new(),
            active: Vec::new(),
            triangles: Vec::new(),
            triangle_samples: usize::MAX,
            triangle_points_hash: 0,
            clips: Vec::new(),
        }
    }

    /// Decoded clocks. Scratch starts empty and is rebuilt on the next evaluation.
    #[must_use]
    pub fn from_states(states: Vec<ClipState>) -> Self {
        Self {
            states,
            order: Vec::new(),
            weights: Vec::new(),
            active: Vec::new(),
            triangles: Vec::new(),
            triangle_samples: usize::MAX,
            triangle_points_hash: 0,
            clips: Vec::new(),
        }
    }
}

/// Runtime instance of a graph node tree: holds only mutable playback state; definitions
/// stay in the [`AnimGraph`] asset.
#[derive(Clone, Debug)]
pub enum GraphInstance {
    /// Clip playback.
    Clip(ClipState),
    /// 1D blend space. See [`BlendRuntime`].
    BlendSpace1D(BlendRuntime),
    /// 2D blend space. See [`BlendRuntime`].
    BlendSpace2D(BlendRuntime),
    /// State machine.
    StateMachine(Box<StateMachineInstance>),
    /// Blend node children.
    Blend {
        /// Weight-0 child.
        a: Box<GraphInstance>,
        /// Weight-1 child.
        b: Box<GraphInstance>,
    },
    /// Additive node children.
    Additive {
        /// Base.
        base: Box<GraphInstance>,
        /// Deltas.
        additive: Box<GraphInstance>,
    },
    /// Layer node children.
    Layer {
        /// Base.
        base: Box<GraphInstance>,
        /// Layers.
        layers: Vec<GraphInstance>,
        /// Per-layer inertialized weight state (parallel to `layers`).
        weights: Vec<WeightInertia>,
        /// Cached per-layer joint weights. Rebuilt when the skeleton's joints or parents or a
        /// layer's mask change.
        ///
        /// A `GraphInstance::Layer { .. }` literal written outside this crate must set it
        /// (or use `..`). It is runtime cache and is not part of the pose snapshot.
        mask_weights: Vec<Vec<f32>>,
        /// Cache key `mask_weights` was built for: a hash of the skeleton's joint names and
        /// parents and of every layer's mask (it was the joint count alone, which kept a stale
        /// mask after an edit). `usize::MAX` until the first evaluation.
        mask_joints: usize,
    },
    /// Stateless wrapper (two-bone IK, look-at, root-motion override).
    Wrap(Box<GraphInstance>),
    /// A locomotion warp node (`StrideWarp` or `OrientationWarp`, format v4, ADR-8540): the inner
    /// graph plus the node's smoothed ratio, last turn and kept buffers.
    Warp {
        /// The inner graph's instance.
        inner: Box<GraphInstance>,
        /// The node's state (read it for the ratio and the turn).
        state: Box<crate::warp_nodes::WarpNodeState>,
    },
}

impl GraphInstance {
    /// Build the default instance for a node tree.
    pub fn from_node(node: &AnimNode) -> Self {
        match node {
            AnimNode::Clip(_) => GraphInstance::Clip(ClipState::default()),
            AnimNode::BlendSpace1D(n) => {
                GraphInstance::BlendSpace1D(BlendRuntime::from_count(n.space.samples.len() + 1))
            }
            AnimNode::BlendSpace2D(n) => {
                GraphInstance::BlendSpace2D(BlendRuntime::from_count(n.space.samples.len() + 1))
            }
            AnimNode::StateMachine(sm) => {
                GraphInstance::StateMachine(Box::new(StateMachineInstance::from_def(sm)))
            }
            AnimNode::Blend(def) => GraphInstance::Blend {
                a: Box::new(GraphInstance::from_node(&def.a)),
                b: Box::new(GraphInstance::from_node(&def.b)),
            },
            AnimNode::Additive(def) => GraphInstance::Additive {
                base: Box::new(GraphInstance::from_node(&def.base)),
                additive: Box::new(GraphInstance::from_node(&def.additive)),
            },
            AnimNode::Layer(n) => GraphInstance::Layer {
                base: Box::new(GraphInstance::from_node(&n.base)),
                layers: n
                    .layers
                    .iter()
                    .map(|l| GraphInstance::from_node(&l.graph))
                    .collect(),
                weights: vec![WeightInertia::default(); n.layers.len()],
                mask_weights: Vec::new(),
                mask_joints: usize::MAX,
            },
            AnimNode::TwoBoneIK(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.inner))),
            AnimNode::LookAt(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.inner))),
            AnimNode::RootMotion(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.inner))),
            AnimNode::Slot(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.source))),
            AnimNode::ChainIK(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.inner))),
            AnimNode::BodyAim(n) => GraphInstance::Wrap(Box::new(GraphInstance::from_node(&n.inner))),
            AnimNode::StrideWarp(n) => GraphInstance::Warp {
                inner: Box::new(GraphInstance::from_node(&n.inner)),
                state: Box::default(),
            },
            AnimNode::OrientationWarp(n) => GraphInstance::Warp {
                inner: Box::new(GraphInstance::from_node(&n.inner)),
                state: Box::default(),
            },
        }
    }

    /// True when any clip in this (sub)tree has finished (non-looping and at its end). Used
    /// by `ClipFinished` transition conditions.
    pub fn any_clip_finished(&self) -> bool {
        match self {
            GraphInstance::Clip(st) => st.finished,
            GraphInstance::BlendSpace1D(rt) | GraphInstance::BlendSpace2D(rt) => {
                rt.states.iter().any(|s| s.finished)
            }
            GraphInstance::StateMachine(sm) => sm
                .states
                .get(sm.current)
                .is_some_and(GraphInstance::any_clip_finished),
            GraphInstance::Blend { a, b } | GraphInstance::Additive { base: a, additive: b } => {
                a.any_clip_finished() || b.any_clip_finished()
            }
            GraphInstance::Layer { base, layers, .. } => {
                base.any_clip_finished() || layers.iter().any(GraphInstance::any_clip_finished)
            }
            GraphInstance::Wrap(inner) | GraphInstance::Warp { inner, .. } => inner.any_clip_finished(),
        }
    }

    /// True when this instance has the shape `from_node` would build for `node` (same node
    /// kinds, sample, state and layer counts all the way down). A hot-reloaded graph can leave
    /// an instance that no longer fits; `evaluate_graph` rebuilds it then.
    pub fn matches_node(&self, node: &AnimNode) -> bool {
        match (node, self) {
            (AnimNode::Clip(_), GraphInstance::Clip(_)) => true,
            // One state per sample plus the shared clock slot (`from_node`): an instance of any
            // other length belongs to another revision of the space.
            (AnimNode::BlendSpace1D(n), GraphInstance::BlendSpace1D(s)) => {
                n.space.samples.len() + 1 == s.states.len()
            }
            (AnimNode::BlendSpace2D(n), GraphInstance::BlendSpace2D(s)) => {
                n.space.samples.len() + 1 == s.states.len()
            }
            (AnimNode::StateMachine(def), GraphInstance::StateMachine(sm)) => {
                sm.states.len() == def.states.len()
                    && sm
                        .states
                        .iter()
                        .zip(&def.states)
                        .all(|(inst, (_, node))| inst.matches_node(node))
            }
            (AnimNode::Blend(def), GraphInstance::Blend { a, b }) => {
                a.matches_node(&def.a) && b.matches_node(&def.b)
            }
            (AnimNode::Additive(def), GraphInstance::Additive { base, additive }) => {
                base.matches_node(&def.base) && additive.matches_node(&def.additive)
            }
            (
                AnimNode::Layer(n),
                GraphInstance::Layer {
                    base,
                    layers,
                    weights,
                    ..
                },
            ) => {
                base.matches_node(&n.base)
                    && layers.len() == n.layers.len()
                    && weights.len() == n.layers.len()
                    && layers
                        .iter()
                        .zip(&n.layers)
                        .all(|(inst, l)| inst.matches_node(&l.graph))
            }
            (AnimNode::TwoBoneIK(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.inner),
            (AnimNode::LookAt(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.inner),
            (AnimNode::RootMotion(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.inner),
            (AnimNode::Slot(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.source),
            (AnimNode::ChainIK(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.inner),
            (AnimNode::BodyAim(n), GraphInstance::Wrap(inner)) => inner.matches_node(&n.inner),
            (AnimNode::StrideWarp(n), GraphInstance::Warp { inner, .. }) => inner.matches_node(&n.inner),
            (AnimNode::OrientationWarp(n), GraphInstance::Warp { inner, .. }) => inner.matches_node(&n.inner),
            _ => false,
        }
    }

    /// Reset playback to the initial state (keeps parameter values, which live elsewhere).
    pub fn reset(node: &AnimNode, instance: &mut Self) {
        *instance = GraphInstance::from_node(node);
    }

    /// Name of the current state when this sub-tree is a state machine. The definition must
    /// be the machine definition matching this instance.
    pub fn state_name(&self, def: &StateMachineDef) -> Option<String> {
        match self {
            GraphInstance::StateMachine(sm) => sm.state_name(def),
            GraphInstance::Wrap(inner) | GraphInstance::Warp { inner, .. } => inner.state_name(def),
            _ => None,
        }
    }
}

impl StateMachineInstance {
    /// Build the default instance (current = initial state).
    pub fn from_def(def: &StateMachineDef) -> Self {
        let current = def.state_index(&def.initial).unwrap_or(0);
        StateMachineInstance {
            states: def
                .states
                .iter()
                .map(|(_, node)| GraphInstance::from_node(node))
                .collect(),
            current,
            transition: None,
            entered_initial: false,
            inertia: Inertializer::default(),
            tracks_history: def.transitions.iter().any(|t| t.blend.is_inertial()),
            prev_output: Pose::default(),
            history_valid: false,
        }
    }

    /// The running transition's blend mode, if a transition or an inertial decay is running.
    #[must_use]
    pub fn active_blend(&self) -> Option<TransitionBlend> {
        self.transition.as_ref().map(|t| t.blend)
    }

    /// Name of the current state.
    pub fn state_name(&self, def: &StateMachineDef) -> Option<String> {
        def.states.get(self.current).map(|(name, _)| name.clone())
    }
}

/// A two-bone chain resolved for one graph node and one skeleton.
#[derive(Clone, Copy, Debug)]
struct TwoBoneSlot {
    /// Address of the two-bone node.
    node: usize,
    /// Address of the skeleton.
    skeleton: usize,
    /// The evaluation that last used this slot.
    used: u64,
    /// Resolved joint ids.
    chain: IkChain,
}

impl crate::ik_nodes::Stamped for TwoBoneSlot {
    fn key(&self) -> (usize, usize) {
        (self.node, self.skeleton)
    }
    fn used(&self) -> u64 {
        self.used
    }
}

/// A look-at joint resolved for one graph node and one skeleton.
#[derive(Clone, Copy, Debug)]
struct LookSlot {
    /// Address of the look-at node.
    node: usize,
    /// Address of the skeleton.
    skeleton: usize,
    /// The evaluation that last used this slot.
    used: u64,
    /// Resolved joint id.
    joint: u32,
}

impl crate::ik_nodes::Stamped for LookSlot {
    fn key(&self) -> (usize, usize) {
        (self.node, self.skeleton)
    }
    fn used(&self) -> u64 {
        self.used
    }
}

/// Buffers one rig's evaluation reuses on the next frame.
///
/// The animator (or the caller of [`EvalContext::with_reuse`]) keeps this between evaluations.
/// A fresh context still builds its bind pose and resolves names; the next evaluation of the
/// same skeleton does not.
#[derive(Clone, Debug, Default)]
pub struct EvalReuse {
    pub(crate) bind: Pose,
    pub(crate) bind_skeleton: usize,
    pub(crate) fk: Vec<Mat4>,
    pub(crate) curves: Vec<(String, f32)>,
    pub(crate) curve_weights: Vec<f32>,
    two_bone: Vec<TwoBoneSlot>,
    look: Vec<LookSlot>,
    ik_nodes: crate::ik_nodes::IkNodeCache,
    /// Skeleton-space joint positions recorded before each `TwoBoneIK` solve this evaluation.
    /// Foot placement reads them next frame so a graph's plant lock sees the clip, not the
    /// solved foot. Cleared at the start of every evaluation; capacity stays.
    pub(crate) pre_ik: Vec<(u32, Vec3)>,
    /// The IK debug flag and the latest evaluation's residual captures (`anim.ik_report`).
    ik_debug: crate::debug::IkDebugLog,
}

/// True when `pose` is exactly `skeleton`'s bind pose.
fn bind_matches(pose: &Pose, skeleton: &Skeleton) -> bool {
    let n = skeleton.bind_pose.len();
    pose.translations.len() == n
        && pose.rotations.len() == n
        && pose.scales.len() == n
        && skeleton
            .bind_pose
            .iter()
            .zip(pose.translations.iter().zip(&pose.rotations).zip(&pose.scales))
            .all(|(bind, ((t, r), s))| *t == bind.translation && *r == bind.rotation && *s == bind.scale)
}

/// True when `joint` still names `name` in `skeleton`: a cached lookup keyed by addresses is
/// only trusted after this check (a node edited in place, or a graph or skeleton reloaded at a
/// freed address, kept the same key and served the old joints; audit corr-animation).
fn joint_named(skeleton: &Skeleton, joint: u32, name: &str) -> bool {
    skeleton
        .joint_names
        .get(joint as usize)
        .is_some_and(|n| n == name)
}

impl EvalReuse {
    /// What the last `ChainIK` and `BodyAim` graph nodes left on this rig (format v4).
    #[must_use]
    pub fn ik_node_residuals(&self) -> crate::ik_nodes::IkNodeResiduals {
        self.ik_nodes.residuals()
    }

    /// Turn per-node IK residual capture on or off (`anim.ik_report`, ADR-8708). While on,
    /// every evaluation measures each solved `TwoBoneIK`, `LookAt`, `ChainIK` and `BodyAim`
    /// node and [`crate::debug::graph_debug_with_ik`] reports it; while off (the default)
    /// the evaluator measures nothing.
    pub fn set_ik_debug(&mut self, on: bool) {
        self.ik_debug.on = on;
        if !on {
            self.ik_debug.records.clear();
        }
    }

    /// Whether IK residual capture is on.
    #[must_use]
    pub fn ik_debug(&self) -> bool {
        self.ik_debug.on
    }

    /// Residual measurements this rig has made (saturating). It stays 0 while capture is
    /// off: evaluation with the flag off does no IK debug work.
    #[must_use]
    pub fn ik_debug_captures(&self) -> u64 {
        self.ik_debug.captures
    }

    pub(crate) fn ik_debug_log(&self) -> &crate::debug::IkDebugLog {
        &self.ik_debug
    }

    /// Start one evaluation: the IK caches stamp the slots this evaluation uses and drop
    /// stale ones past their size (ADR-8735; a live slot is never evicted, so a graph with more
    /// than 64 IK nodes of a kind does not resolve them again every frame).
    pub(crate) fn begin_evaluation(&mut self) {
        self.ik_nodes.begin_evaluation();
        let now = self.ik_nodes.now();
        crate::ik_nodes::prune(&mut self.two_bone, now);
        crate::ik_nodes::prune(&mut self.look, now);
    }

    /// The chain resolved for this node, if the skeleton is the one it was resolved on (the
    /// slot is stamped as used).
    fn two_bone(&mut self, node: usize, skeleton: usize) -> Option<IkChain> {
        let now = self.ik_nodes.now();
        self.two_bone
            .iter_mut()
            .find(|slot| slot.node == node && slot.skeleton == skeleton)
            .map(|slot| {
                slot.used = now;
                slot.chain
            })
    }

    /// Remember a resolved two-bone chain.
    fn store_two_bone(&mut self, node: usize, skeleton: usize, chain: IkChain) {
        // Keys are addresses: a graph or skeleton reloaded at a new address adds a slot, and
        // the old one goes stale and is replaced (`ik_nodes::store`).
        let used = self.ik_nodes.now();
        crate::ik_nodes::store(
            &mut self.two_bone,
            TwoBoneSlot {
                node,
                skeleton,
                used,
                chain,
            },
            used,
        );
    }

    /// The look-at joint resolved for this node, if the skeleton matches (the slot is stamped
    /// as used).
    fn look(&mut self, node: usize, skeleton: usize) -> Option<u32> {
        let now = self.ik_nodes.now();
        self.look
            .iter_mut()
            .find(|slot| slot.node == node && slot.skeleton == skeleton)
            .map(|slot| {
                slot.used = now;
                slot.joint
            })
    }

    /// Remember a resolved look-at joint.
    fn store_look(&mut self, node: usize, skeleton: usize, joint: u32) {
        let used = self.ik_nodes.now();
        crate::ik_nodes::store(
            &mut self.look,
            LookSlot {
                node,
                skeleton,
                used,
                joint,
            },
            used,
        );
    }

    /// Remember the animated (pre-solve) object position of `joint`. A joint recorded twice
    /// in one evaluation keeps the later sample.
    pub(crate) fn note_pre_ik(&mut self, joint: u32, position: Vec3) {
        if let Some(slot) = self.pre_ik.iter_mut().find(|(id, _)| *id == joint) {
            slot.1 = position;
        } else {
            self.pre_ik.push((joint, position));
        }
    }
}

/// Evaluation context: read-only world for one animator's evaluation plus the accumulated
/// outputs. Scratch poses are pooled in the animator to avoid per-frame allocation.
pub struct EvalContext<'a> {
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// Clip lookup by name.
    pub clips: &'a ClipLibrary,
    /// Parameters.
    pub params: &'a ParameterSet,
    /// Frame delta in seconds (already scaled by the animator's speed).
    pub delta: f32,
    /// The rig entity (for socket lookups).
    pub rig: Entity,
    /// Object-space socket positions from the previous applied frame.
    pub socket_targets: &'a HashMap<(Entity, String), Vec3>,

    /// Notifies fired this evaluation.
    pub notifies: Vec<Notify>,
    /// State enter/exit events, in order.
    pub state_events: Vec<StateEvent>,
    /// Curve values this evaluation (name → value). A curve sampled by several clips holds
    /// their weighted average. In a context reused across evaluations, an entry may be a
    /// leftover from an earlier evaluation with zero weight this frame: the parallel private
    /// weights tell those apart for the engine path, but an external reader cannot — treat a
    /// reused context's entries as "this frame or earlier".
    pub curves: Vec<(String, f32)>,
    /// Accumulated weight of each entry of `curves` (parallel to it).
    curve_weights: Vec<f32>,

    /// Root motion of this evaluation, `(translation, rotation)`: every node blends its
    /// children's deltas with the same weights it blends their poses (a blend space at 50/50
    /// moves at the average speed), and clips without root motion contribute zero.
    pub root_motion: (Vec3, Quat),
    rm_mode_override: Option<RootMotionMode>,
    /// The graph's morph weights, set when the evaluation completes.
    morphs: MorphMix,
    scratch: Vec<Pose>,
    /// Per-clip curve sample buffer, reused across clips and (when the caller puts it back)
    /// across frames.
    curve_sample: Vec<(String, f32)>,
    /// Per-clip node-morph sample buffer.
    node_raw: Vec<(u32, u32, f32)>,
    /// Morph list buffers reused by blends.
    morph_pools: MorphPools,
    /// Chain matrices reused by two-bone IK and look-at.
    ik_globals: Vec<Mat4>,
    /// The skeleton's bind pose, built once per evaluation (every clip samples on top of it).
    bind: Pose,
    /// Names of every notify crossed this evaluation, audible or not: state-machine `Notify`
    /// conditions read this, so a sub-graph at weight 0 still advances on its own notifies.
    notify_log: Vec<String>,

    /// Trigger parameters consumed by transitions that fired this evaluation; the calling
    /// system applies the consumption to the animator's parameter set (parameters are
    /// borrowed immutably during evaluation).
    pub consumed_triggers: Vec<String>,

    /// Declared sync groups (`None` = clips keep their own clocks, as before ADR-1465).
    sync_setup: Option<&'a SyncSetup>,
    /// Sync-group runtime state, carried between frames by the animator.
    sync: SyncGroups,
    /// Montage poses per slot for `Slot` nodes (spec 34 M9); `None` = the montage stage runs
    /// after the graph only.
    slots: Option<&'a SlotLayers>,
    /// Slots a `Slot` node blended this evaluation: the montage stage skips their pose blend
    /// (`MontagePlayer::finish_frame`).
    pub consumed_slots: Vec<String>,
    /// Bind pose, curve names, mask weights and IK buffers carried between frames.
    reuse: EvalReuse,
}

/// What [`EvalContext::finish`] hands back: the reusable scratch pool and the normalised
/// morph weights.
pub struct FinishedEval {
    /// Reusable scratch pose pool.
    pub scratch: Vec<Pose>,
    /// Morph weights of tracks that apply to every morph mesh, blended through the graph like
    /// the pose (a target one side of a blend lacks counts as 0), sorted by target.
    pub morphs: Vec<(u32, f32)>,
    /// Normalised morph weights of per-node tracks: `(node index, target index, weight)`,
    /// sorted by node then target (ADR-297).
    pub node_morphs: Vec<(u32, u32, f32)>,
    /// Curve-name sample buffer. Put it back with [`EvalContext::with_eval_scratch`].
    pub curve_sample: Vec<(String, f32)>,
    /// Recorded curve values (names kept). Put them back with [`EvalContext::with_eval_scratch`].
    pub curves: Vec<(String, f32)>,
    /// Node-morph sample buffer. Put it back with [`EvalContext::with_eval_scratch`].
    pub node_raw: Vec<(u32, u32, f32)>,
    /// Shared-morph list pool. Put it back with [`EvalContext::with_eval_scratch`].
    pub morph_shared_pool: Vec<Vec<(u32, f32)>>,
    /// Per-node morph list pool. Put it back with [`EvalContext::with_eval_scratch`].
    pub morph_nodes_pool: Vec<Vec<((u32, u32), f32)>>,
    /// IK chain matrices. Put them back with [`EvalContext::with_eval_scratch`].
    pub ik_globals: Vec<Mat4>,
}

/// A state-machine enter/exit event.
#[derive(Clone, Debug, PartialEq)]
pub struct StateEvent {
    /// State exited (`None` for the initial enter event).
    pub from: Option<String>,
    /// State entered.
    pub to: String,
}

/// A named collection of clips an animator can play. Shared via `Arc`.
#[derive(Clone, Debug, Default)]
pub struct ClipLibrary {
    clips: HashMap<String, Arc<AnimationClip>>,
}

impl ClipLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a clip.
    pub fn insert(&mut self, clip: AnimationClip) {
        self.clips.insert(clip.name.clone(), Arc::new(clip));
    }

    /// Look up a clip by name.
    pub fn get(&self, name: &str) -> Option<&Arc<AnimationClip>> {
        self.clips.get(name)
    }

    /// Number of clips.
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// True when the library holds no clips.
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Clip names, unordered.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.clips.keys().map(|s| s.as_str())
    }
}

impl<'a> EvalContext<'a> {
    /// New context for one evaluation. `scratch` is the animator's pooled pose buffer.
    pub fn new(
        skeleton: &'a Skeleton,
        clips: &'a ClipLibrary,
        params: &'a ParameterSet,
        delta: f32,
        rig: Entity,
        socket_targets: &'a HashMap<(Entity, String), Vec3>,
        scratch: Vec<Pose>,
    ) -> Self {
        let mut ctx = Self::blank(skeleton, clips, params, delta, rig, socket_targets, scratch);
        ctx.bind = skeleton.bind_pose_pose();
        ctx
    }

    /// Context with an empty bind pose.
    ///
    /// [`EvalContext::new`] fills the bind pose. The evaluator calls
    /// [`EvalContext::install_reuse`] so a repeat evaluation moves the cached pose in
    /// instead of building another one.
    pub(crate) fn blank(
        skeleton: &'a Skeleton,
        clips: &'a ClipLibrary,
        params: &'a ParameterSet,
        delta: f32,
        rig: Entity,
        socket_targets: &'a HashMap<(Entity, String), Vec3>,
        scratch: Vec<Pose>,
    ) -> Self {
        EvalContext {
            skeleton,
            clips,
            params,
            delta,
            rig,
            socket_targets,
            notifies: Vec::new(),
            state_events: Vec::new(),
            curves: Vec::new(),
            curve_weights: Vec::new(),
            root_motion: (Vec3::ZERO, Quat::IDENTITY),
            rm_mode_override: None,
            morphs: MorphMix::default(),
            scratch,
            curve_sample: Vec::new(),
            node_raw: Vec::new(),
            morph_pools: MorphPools::default(),
            ik_globals: Vec::new(),
            bind: Pose::default(),
            notify_log: Vec::new(),
            consumed_triggers: Vec::new(),
            sync_setup: None,
            sync: SyncGroups::default(),
            slots: None,
            consumed_slots: Vec::new(),
            reuse: EvalReuse::default(),
        }
    }

    /// Move `reuse` from the previous frame into this context.
    ///
    /// When `reuse` already holds this skeleton's bind pose, that pose is moved in and
    /// [`Skeleton::bind_pose_pose`] is not called. Curve names already in `reuse` stay allocated.
    /// Call this on an [`EvalContext::blank`] context, before evaluation.
    pub(crate) fn install_reuse(&mut self, mut reuse: EvalReuse) {
        // Last frame's tips were copied onto `FootPlacement` at the end of that evaluation.
        reuse.pre_ik.clear();
        reuse.ik_debug.begin();
        let skeleton_ptr = std::ptr::from_ref(self.skeleton) as usize;
        // The address alone is not enough: a skeleton reloaded at a freed address must not get
        // the old bind pose, so the kept pose is compared with this skeleton's (no allocation).
        let bind = if reuse.bind_skeleton == skeleton_ptr
            && reuse.bind.len() == self.skeleton.len()
            && bind_matches(&reuse.bind, self.skeleton)
        {
            std::mem::take(&mut reuse.bind)
        } else {
            let pose = self.skeleton.bind_pose_pose();
            reuse.bind_skeleton = skeleton_ptr;
            pose
        };
        reuse.curve_weights.fill(0.0);
        self.bind = bind;
        self.curves = std::mem::take(&mut reuse.curves);
        self.curve_weights = std::mem::take(&mut reuse.curve_weights);
        self.reuse = reuse;
    }

    /// Install buffers kept from the previous evaluation of this graph.
    ///
    /// [`EvalContext::new`] has already built a bind pose. This moves curve names, mask weights
    /// and the IK matrix buffer in so the evaluation can reuse them. Hand the buffers back with
    /// [`EvalContext::take_reuse`] before [`EvalContext::finish`].
    #[must_use]
    pub fn with_reuse(mut self, mut reuse: EvalReuse) -> Self {
        reuse.ik_debug.begin();
        reuse.curve_weights.fill(0.0);
        self.curves = std::mem::take(&mut reuse.curves);
        self.curve_weights = std::mem::take(&mut reuse.curve_weights);
        self.reuse = reuse;
        self
    }

    /// Take the reusable buffers back, including this evaluation's curve names and bind pose.
    pub fn take_reuse(&mut self) -> EvalReuse {
        let mut reuse = std::mem::take(&mut self.reuse);
        reuse.curves = std::mem::take(&mut self.curves);
        reuse.curve_weights = std::mem::take(&mut self.curve_weights);
        reuse.bind = std::mem::take(&mut self.bind);
        reuse
    }

    /// Hand `Slot` nodes this frame's montage poses ([`crate::MontagePlayer::slot_layers`],
    /// spec 34 M9). Without it a `Slot` node passes its source through.
    #[must_use]
    pub fn with_slots(mut self, layers: Option<&'a SlotLayers>) -> Self {
        self.slots = layers;
        self
    }

    /// Enable cross-node sync groups for this evaluation (`docs/specs/17-animation.md` § 4b):
    /// `setup` declares the groups and markers, `state` is the animator's sync state from the
    /// previous frame (get it back with [`EvalContext::take_sync_state`]).
    #[must_use]
    pub fn with_sync(mut self, setup: Option<&'a SyncSetup>, state: SyncGroups) -> Self {
        self.sync_setup = setup;
        self.sync = state;
        self
    }

    /// The sync-group state after this evaluation (leaders elected for the next frame).
    pub fn take_sync_state(&mut self) -> SyncGroups {
        std::mem::take(&mut self.sync)
    }

    /// Consume the context, returning the scratch pool for reuse next frame and the graph's
    /// blended morph weights.
    pub fn finish(mut self) -> FinishedEval {
        let node_morphs = self
            .morphs
            .nodes
            .iter()
            .map(|&((node, target), value)| (node, target, value))
            .collect();
        self.morph_pools
            .give_nodes(std::mem::take(&mut self.morphs.nodes));
        FinishedEval {
            scratch: self.scratch,
            morphs: self.morphs.shared,
            node_morphs,
            curve_sample: self.curve_sample,
            curves: self.curves,
            node_raw: self.node_raw,
            morph_shared_pool: self.morph_pools.shared,
            morph_nodes_pool: self.morph_pools.nodes,
            ik_globals: self.ik_globals,
        }
    }

    /// Install scratch buffers returned by the previous [`EvalContext::finish`].
    ///
    /// Curve weights are reset so the first sample of a name this frame replaces the
    /// stored value. The name strings stay.
    #[must_use]
    pub fn with_eval_scratch(
        mut self,
        curve_sample: Vec<(String, f32)>,
        curves: Vec<(String, f32)>,
        node_raw: Vec<(u32, u32, f32)>,
        morph_shared_pool: Vec<Vec<(u32, f32)>>,
        morph_nodes_pool: Vec<Vec<((u32, u32), f32)>>,
        ik_globals: Vec<Mat4>,
    ) -> Self {
        self.curve_sample = curve_sample;
        self.curves = curves;
        self.curve_weights.clear();
        self.curve_weights.resize(self.curves.len(), 0.0);
        self.node_raw = node_raw;
        self.morph_pools.shared = morph_shared_pool;
        self.morph_pools.nodes = morph_nodes_pool;
        self.ik_globals = ik_globals;
        self
    }

    /// Seconds that transitions, inertial decays and layer weights advance this evaluation:
    /// they run forward in time even while clips play in reverse.
    fn blend_dt(&self) -> f32 {
        crate::num::finite_or(self.delta, 0.0).abs()
    }

    fn take_pose(&mut self, len: usize) -> Pose {
        let mut pose = self.scratch.pop().unwrap_or_default();
        if pose.len() != len {
            pose.translations.resize(len, Vec3::ZERO);
            pose.rotations.resize(len, Quat::IDENTITY);
            pose.scales.resize(len, Vec3::ONE);
        }
        pose
    }

    fn give_pose(&mut self, pose: Pose) {
        self.scratch.push(pose);
    }

    /// Fold a sampled curve value into `curves` as a running weighted average.
    fn record_curve(&mut self, name: &str, value: f32, weight: f32) {
        // `curves` is public; keep the weights in step if a caller edited it.
        self.curve_weights.resize(self.curves.len(), 0.0);
        let weight = weight.max(0.0);
        match self.curves.iter().position(|(n, _)| n == name) {
            Some(index) => {
                let previous = self.curve_weights[index];
                let total = previous + weight;
                let current = &mut self.curves[index].1;
                if previous <= 0.0 && weight > 0.0 {
                    // The first sample this frame over a slot kept from an earlier frame:
                    // `c + (v - c) * 1` is not always `v` in f32, which made the value depend
                    // on the previous frame's (audit corr-animation).
                    *current = value;
                } else if total > 0.0 {
                    *current += (value - *current) * (weight / total);
                }
                self.curve_weights[index] = total;
            }
            None => {
                crate::hot_count::count_curve_name_clone();
                self.curves.push((name.to_string(), value));
                self.curve_weights.push(weight);
            }
        }
    }
}

/// The cache key of a layer node's resolved masks: FNV-1a over the skeleton's joint names and
/// parents and every layer's mask. Never `usize::MAX` (the "not built" marker).
fn layer_mask_key(skeleton: &Skeleton, layers: &[crate::graph::LayerEntry]) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(&(skeleton.len() as u64).to_le_bytes());
    for name in &skeleton.joint_names {
        mix(name.as_bytes());
        mix(&[0xff]);
    }
    for parent in &skeleton.parents {
        mix(&parent.unwrap_or(u32::MAX).to_le_bytes());
    }
    for entry in layers {
        mix(&[0xfe, u8::from(entry.mask.include_children)]);
        for joint in &entry.mask.joints {
            mix(joint.as_bytes());
            mix(&[0xff]);
        }
    }
    let key = hash as usize;
    if key == usize::MAX { key ^ 1 } else { key }
}

/// Evaluate a whole graph into `out`. The evaluation's root motion lands in
/// [`EvalContext::root_motion`].
///
/// # Errors
///
/// Returns the first [`EvalError`] encountered (missing clips are re-reported every frame;
/// the calling system logs them).
#[tracing::instrument(
    target = "regia_animation",
    name = "evaluate_graph",
    skip_all,
    fields(joints = ctx.skeleton.len())
)]
pub fn evaluate_graph(
    graph: &AnimGraph,
    instance: &mut GraphInstance,
    ctx: &mut EvalContext,
    out: &mut Pose,
) -> Result<(), EvalError> {
    if !instance.matches_node(&graph.root) {
        // The graph was replaced by one of a different shape: restart rather than evaluate
        // against state that belongs to the old tree (which would write nothing).
        GraphInstance::reset(&graph.root, instance);
    }
    if ctx.sync_setup.is_some() {
        ctx.sync.begin_frame();
    }
    ctx.reuse.begin_evaluation();
    let result = eval_into(&graph.root, instance, ctx, out, Weight::FULL);
    if ctx.sync_setup.is_some() {
        ctx.sync.end_frame();
    }
    let result = result?;
    ctx.root_motion = (result.root.translation, result.root.rotation);
    ctx.morphs = result.morphs;
    Ok(())
}

/// A step in clip seconds as the normalised step `ClipPlay::sync` takes.
fn normalised(step: ClipStep, duration: f32) -> ClipStep {
    if duration > 0.0 {
        ClipStep {
            prev: step.prev / duration,
            time: step.time / duration,
            loops: step.loops,
        }
    } else {
        ClipStep::default()
    }
}

/// Evaluate a single-clip animator source (the `AnimatorSource::SingleClip` path).
///
/// # Errors
///
/// Returns an error only for a structurally invalid instance.
#[tracing::instrument(
    target = "regia_animation",
    name = "evaluate_single_clip",
    skip_all,
    fields(joints = ctx.skeleton.len())
)]
pub fn evaluate_single_clip(
    clip: &AnimationClip,
    looping: bool,
    instance: &mut GraphInstance,
    ctx: &mut EvalContext,
    out: &mut Pose,
) -> Result<(), EvalError> {
    match instance {
        GraphInstance::Clip(st) => {
            let result = eval_clip(clip, st, ctx, out, Weight::FULL, ClipPlay::own(looping, 1.0))?;
            ctx.root_motion = (result.root.translation, result.root.rotation);
            ctx.morphs = result.morphs;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Evaluate one node into `out` at the given accumulated `weight` (root = full). Returns the
/// node's root motion, combined from its children like its pose.
fn eval_into(
    node: &AnimNode,
    instance: &mut GraphInstance,
    ctx: &mut EvalContext,
    out: &mut Pose,
    weight: Weight,
) -> Result<NodeOut, EvalError> {
    match node {
        AnimNode::Clip(def) => {
            let GraphInstance::Clip(st) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let clip = ctx
                .clips
                .get(&def.clip)
                .ok_or_else(|| EvalError::MissingClip(def.clip.clone()))?;
            let mut play = ClipPlay::own(def.looping, def.speed);
            // A declared sync group: the leader keeps its clock, followers take theirs from it.
            let setup = ctx.sync_setup;
            let group = def
                .sync_group
                .as_deref()
                .filter(|g| setup.is_some_and(|s| s.declares(g)));
            let (Some(group), Some(setup)) = (group, setup) else {
                return eval_clip(clip, st, ctx, out, weight, play);
            };
            let query = SyncQuery {
                member: &def.clip,
                duration: clip.duration,
                own_time: st.time,
                delta: ctx.delta,
                markers: true,
            };
            match ctx.sync.play(setup, group, query) {
                SyncPlay::Own => {}
                SyncPlay::Lead(step) => play.sync = Some(normalised(step, clip.duration)),
                SyncPlay::Follow(step) => {
                    play.sync = Some(normalised(step, clip.duration));
                    play.notifies = false;
                }
            }
            let result = eval_clip(clip, st, ctx, out, weight, play)?;
            ctx.sync.record(
                setup,
                group,
                SyncMember {
                    clip: &def.clip,
                    weight: weight.pose,
                    time: st.time,
                    speed: def.speed,
                    looping: def.looping,
                    duration: clip.duration,
                },
            );
            Ok(result)
        }
        AnimNode::BlendSpace1D(def) => {
            let GraphInstance::BlendSpace1D(runtime) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let value = ctx.params.float(&def.space.param);
            def.space
                .weights_into(value, &mut runtime.order, &mut runtime.weights);
            let count = def.space.samples.len();
            eval_blendspace(
                |i| def.space.samples[i].clip.as_str(),
                count,
                runtime,
                def.sync_group.as_deref(),
                ctx,
                out,
                weight,
            )
        }
        AnimNode::BlendSpace2D(def) => {
            let GraphInstance::BlendSpace2D(runtime) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let x = ctx.params.float(&def.space.param_x);
            let y = ctx.params.float(&def.space.param_y);
            def.space.weights_into(
                [x, y],
                &mut runtime.triangles,
                &mut runtime.triangle_samples,
                &mut runtime.triangle_points_hash,
                &mut runtime.weights,
            );
            let count = def.space.samples.len();
            eval_blendspace(
                |i| def.space.samples[i].clip.as_str(),
                count,
                runtime,
                def.sync_group.as_deref(),
                ctx,
                out,
                weight,
            )
        }
        AnimNode::StateMachine(def) => {
            let GraphInstance::StateMachine(sm) = instance else {
                return Ok(NodeOut::ZERO);
            };
            eval_state_machine(def, sm, ctx, out, weight)
        }
        AnimNode::Blend(def) => {
            let GraphInstance::Blend { a, b } = instance else {
                return Ok(NodeOut::ZERO);
            };
            let alpha = unit_weight(ctx.params.float(&def.alpha_param));
            let root_a = eval_into(&def.a, a, ctx, out, weight * (1.0 - alpha))?;
            let mut tmp = ctx.take_pose(out.len());
            let root_b = eval_into(&def.b, b, ctx, &mut tmp, weight * alpha)?;
            out.blend_from(&tmp, alpha);
            ctx.give_pose(tmp);
            Ok(root_a.lerp(root_b, alpha, &mut ctx.morph_pools))
        }
        AnimNode::Additive(def) => {
            let GraphInstance::Additive { base, additive } = instance else {
                return Ok(NodeOut::ZERO);
            };
            let w = unit_weight(ctx.params.float(&def.weight_param));
            let root = eval_into(&def.base, base, ctx, out, weight)?;
            // A weight parameter of 0 contributes nothing, so the additive child is not
            // sampled and its clip clock does not advance (spec 17 does not require it to).
            if w <= 0.0 {
                return Ok(root);
            }
            let mut tmp = ctx.take_pose(out.len());
            let added = eval_into(&def.additive, additive, ctx, &mut tmp, weight * w)?;
            // Sampling resets to the bind pose, so the additive input is a full pose.
            // Apply the delta from bind: a clip that holds the bind pose adds nothing,
            // and a delta authored against an identity bind is unchanged.
            let mut delta = ctx.take_pose(out.len());
            tmp.delta_into(&ctx.bind, &mut delta);
            out.additive_from(&delta, w);
            ctx.give_pose(delta);
            ctx.give_pose(tmp);
            Ok(root.add_scaled(added, w, &mut ctx.morph_pools))
        }
        AnimNode::Layer(def) => {
            let GraphInstance::Layer {
                base,
                layers: layer_insts,
                weights: weight_states,
                mask_weights,
                mask_joints,
            } = instance
            else {
                return Ok(NodeOut::ZERO);
            };
            let mask_key = layer_mask_key(ctx.skeleton, &def.layers);
            if *mask_joints != mask_key || mask_weights.len() != def.layers.len() {
                mask_weights.clear();
                mask_weights.resize(def.layers.len(), Vec::new());
                for (index, entry) in def.layers.iter().enumerate() {
                    crate::hot_count::count_mask_resolve();
                    let mask = entry.mask.resolve(&ctx.skeleton.joint_names);
                    mask.weights_into(
                        &ctx.skeleton.parents,
                        ctx.skeleton.len(),
                        &mut mask_weights[index],
                    );
                }
                *mask_joints = mask_key;
            }
            let mut root = eval_into(&def.base, base, ctx, out, weight)?;
            // A layer's root motion counts as far as its mask covers the skeleton root.
            let root_joint = ctx.skeleton.parents.iter().position(Option::is_none);
            for (index, entry) in def.layers.iter().enumerate() {
                let Some(layer_inst) = layer_insts.get_mut(index) else {
                    break;
                };
                let target_weight = unit_weight(ctx.params.float(&entry.weight_param));
                // Graphs built in code skip load validation: a NaN easing time means no easing.
                let easing = crate::num::finite_or(entry.weight_inertialize, 0.0);
                let layer_weight = match weight_states.get_mut(index) {
                    Some(state) => unit_weight(state.update(target_weight, easing, ctx.blend_dt())),
                    None => target_weight,
                };
                if layer_weight <= 0.0 {
                    continue;
                }
                let mask_row = &mask_weights[index];
                let mut tmp = ctx.take_pose(out.len());
                let layer_root = eval_into(&entry.graph, layer_inst, ctx, &mut tmp, weight * layer_weight)?;
                let root_share =
                    layer_weight * root_joint.and_then(|r| mask_row.get(r)).copied().unwrap_or(0.0);
                // Morphs have no joints to mask: a layer drives the morphs it has, by its weight.
                match entry.mode {
                    LayerMode::Override => {
                        out.masked_blend_from(&tmp, mask_row, layer_weight);
                        let morphs =
                            root.morphs
                                .override_with(&layer_root.morphs, layer_weight, &mut ctx.morph_pools);
                        ctx.morph_pools
                            .recycle(std::mem::replace(&mut root.morphs, morphs));
                        ctx.morph_pools.recycle(layer_root.morphs);
                        root.root = root.root.lerp(layer_root.root, root_share);
                    }
                    LayerMode::Additive => {
                        let mut delta = ctx.take_pose(out.len());
                        tmp.delta_into(&ctx.bind, &mut delta);
                        out.masked_additive_from(&delta, mask_row, layer_weight);
                        ctx.give_pose(delta);
                        let morphs =
                            root.morphs
                                .add_scaled(&layer_root.morphs, layer_weight, &mut ctx.morph_pools);
                        ctx.morph_pools
                            .recycle(std::mem::replace(&mut root.morphs, morphs));
                        ctx.morph_pools.recycle(layer_root.morphs);
                        root.root = root.root.add_scaled(layer_root.root, root_share);
                    }
                }
                ctx.give_pose(tmp);
            }
            Ok(root)
        }
        AnimNode::TwoBoneIK(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let root_motion = eval_into(&def.inner, inner, ctx, out, weight)?;
            let node_key = std::ptr::from_ref(def) as usize;
            let skeleton_key = std::ptr::from_ref(ctx.skeleton) as usize;
            let cached = ctx.reuse.two_bone(node_key, skeleton_key).filter(|c| {
                joint_named(ctx.skeleton, c.root, &def.chain[0])
                    && joint_named(ctx.skeleton, c.mid, &def.chain[1])
                    && joint_named(ctx.skeleton, c.tip, &def.chain[2])
            });
            let chain = if let Some(chain) = cached {
                chain
            } else {
                let root = ctx
                    .skeleton
                    .joint_index(&def.chain[0])
                    .ok_or_else(|| EvalError::MissingJoint(def.chain[0].clone()))?;
                let mid = ctx
                    .skeleton
                    .joint_index(&def.chain[1])
                    .ok_or_else(|| EvalError::MissingJoint(def.chain[1].clone()))?;
                let tip = ctx
                    .skeleton
                    .joint_index(&def.chain[2])
                    .ok_or_else(|| EvalError::MissingJoint(def.chain[2].clone()))?;
                let chain = IkChain { root, mid, tip };
                ctx.reuse.store_two_bone(node_key, skeleton_key, chain);
                chain
            };
            // Clip positions, before this node overwrites the leg. Foot placement (graph output)
            // reads them on the next frame so the plant lock sees the animated ankle, not the
            // foot it already solved.
            ctx.reuse.note_pre_ik(
                chain.root,
                crate::ik::joint_object_point(ctx.skeleton, out, chain.root),
            );
            ctx.reuse.note_pre_ik(
                chain.mid,
                crate::ik::joint_object_point(ctx.skeleton, out, chain.mid),
            );
            ctx.reuse.note_pre_ik(
                chain.tip,
                crate::ik::joint_object_point(ctx.skeleton, out, chain.tip),
            );
            let w = def
                .weight_param
                .as_ref()
                .map_or(1.0, |p| unit_weight(ctx.params.float(p)));
            // An unresolved target (a socket not placed yet, a non-finite parameter) leaves the
            // pose alone instead of reaching for the origin.
            if w > 0.0
                && let Some(target) = resolve_target(&def.target, ctx)
            {
                let pole = def
                    .pole
                    .as_ref()
                    .map(|p| ctx.params.vec3(p))
                    .filter(|p| p.is_finite());
                // Chain-only forward kinematics (animation1) into the reused buffer (animation2).
                let skeleton = ctx.skeleton;
                let fk = &mut ctx.reuse.fk;
                solve_two_bone_with_globals(skeleton, out, &chain, target, pole, w, fk);
                if ctx.reuse.ik_debug.on {
                    let record = crate::debug::two_bone_record(skeleton, out, &chain, target, w, node_key);
                    ctx.reuse.ik_debug.record(record);
                }
            }
            Ok(root_motion)
        }
        AnimNode::LookAt(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let root_motion = eval_into(&def.inner, inner, ctx, out, weight)?;
            let node_key = std::ptr::from_ref(def) as usize;
            let skeleton_key = std::ptr::from_ref(ctx.skeleton) as usize;
            let cached = ctx
                .reuse
                .look(node_key, skeleton_key)
                .filter(|&j| joint_named(ctx.skeleton, j, &def.joint));
            let joint = if let Some(joint) = cached {
                joint
            } else {
                let joint = ctx
                    .skeleton
                    .joint_index(&def.joint)
                    .ok_or_else(|| EvalError::MissingJoint(def.joint.clone()))?;
                ctx.reuse.store_look(node_key, skeleton_key, joint);
                joint
            };
            let bind = ctx
                .skeleton
                .bind_pose
                .get(joint as usize)
                .ok_or_else(|| EvalError::MissingJoint(def.joint.clone()))?
                .rotation;
            let w = def
                .weight_param
                .as_ref()
                .map_or(1.0, |p| unit_weight(ctx.params.float(p)));
            if w > 0.0
                && let Some(target) = resolve_target(&def.target, ctx)
            {
                let skeleton = ctx.skeleton;
                let fk = &mut ctx.reuse.fk;
                let settings = LookAtSettings {
                    max_yaw: def.max_yaw,
                    max_pitch: def.max_pitch,
                    weight: w,
                };
                solve_look_at_with_globals(skeleton, out, joint, bind, target, settings, fk);
                if ctx.reuse.ik_debug.on {
                    let record =
                        crate::debug::look_at_record(skeleton, out, joint, bind, target, settings, node_key);
                    ctx.reuse.ik_debug.record(record);
                }
            }
            Ok(root_motion)
        }
        AnimNode::ChainIK(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let root_motion = eval_into(&def.inner, inner, ctx, out, weight)?;
            let w = def
                .weight_param
                .as_ref()
                .map_or(1.0, |p| unit_weight(ctx.params.float(p)));
            // An unresolved target leaves the pose alone, like the two-bone node.
            if w > 0.0
                && let Some(target) = resolve_target(&def.target, ctx)
            {
                let pole = def
                    .pole
                    .as_ref()
                    .map(|p| ctx.params.vec3(p))
                    .filter(|p| p.is_finite());
                let skeleton = ctx.skeleton;
                let solved = ctx
                    .reuse
                    .ik_nodes
                    .solve_chain_node(def, skeleton, out, target, pole, w)?;
                if solved && ctx.reuse.ik_debug.on {
                    let node = std::ptr::from_ref(def) as usize;
                    let record = ctx.reuse.ik_nodes.chain_record(skeleton, out, target, w, node);
                    ctx.reuse.ik_debug.record(record);
                }
            }
            Ok(root_motion)
        }
        AnimNode::BodyAim(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let root_motion = eval_into(&def.inner, inner, ctx, out, weight)?;
            let w = def
                .weight_param
                .as_ref()
                .map_or(1.0, |p| unit_weight(ctx.params.float(p)));
            if w > 0.0
                && let Some(target) = resolve_target(&def.target, ctx)
            {
                let skeleton = ctx.skeleton;
                ctx.reuse.ik_nodes.solve_aim_node(def, skeleton, out, target, w)?;
                if ctx.reuse.ik_debug.on {
                    let node = std::ptr::from_ref(def) as usize;
                    let record = ctx.reuse.ik_nodes.aim_record(def, w, node);
                    ctx.reuse.ik_debug.record(record);
                }
            }
            Ok(root_motion)
        }
        AnimNode::StrideWarp(def) => {
            let GraphInstance::Warp { inner, state } = instance else {
                return Ok(NodeOut::ZERO);
            };
            // `Rate` plays the whole inner graph on a scaled clock (sync groups inside share
            // it, so their phases hold); `Pose` keeps the clock and scales the stride after.
            let dt = ctx.delta;
            let clip_dt = dt * state.rate(def.settings.mode);
            ctx.delta = clip_dt;
            let result = eval_into(&def.inner, inner, ctx, out, weight);
            ctx.delta = dt;
            let result = result?;
            let frame = crate::warp_nodes::StrideFrame {
                translation: result.root.translation,
                clip_dt,
                dt,
                ground_speed: ctx.params.float(&def.ground_speed_param),
            };
            crate::warp_nodes::stride_after_inner(def, state, ctx.skeleton, out, frame)?;
            Ok(result)
        }
        AnimNode::OrientationWarp(def) => {
            let GraphInstance::Warp { inner, state } = instance else {
                return Ok(NodeOut::ZERO);
            };
            let result = eval_into(&def.inner, inner, ctx, out, weight)?;
            let desired = ctx.params.float(&def.heading_param);
            crate::warp_nodes::orient_after_inner(
                def,
                state,
                ctx.skeleton,
                out,
                result.root.translation,
                desired,
            )?;
            Ok(result)
        }
        AnimNode::RootMotion(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let previous_override = ctx.rm_mode_override;
            ctx.rm_mode_override = Some(def.mode);
            let result = eval_into(&def.inner, inner, ctx, out, weight);
            ctx.rm_mode_override = previous_override;
            result
        }
        AnimNode::Slot(def) => {
            let GraphInstance::Wrap(inner) = instance else {
                return Ok(NodeOut::ZERO);
            };
            let layers = ctx.slots;
            // A full-body layer at weight 1 hides the source completely.
            let covered = layers.is_some_and(|l| {
                l.of(&def.name)
                    .any(|layer| layer.weight >= 1.0 && layer.mask.iter().all(|m| *m >= 1.0))
            });
            let result = if covered && !def.eval_idle_source {
                out.copy_from(&ctx.bind);
                NodeOut::ZERO
            } else {
                eval_into(&def.source, inner, ctx, out, weight)?
            };
            if let Some(layers) = layers {
                for layer in layers.of(&def.name) {
                    out.masked_blend_from(&layer.pose, &layer.mask, layer.weight);
                }
                if !ctx.consumed_slots.contains(&def.name) {
                    ctx.consumed_slots.push(def.name.clone());
                }
            }
            Ok(result)
        }
    }
}

/// Minimum effective weight at which a clip fires its notifies (a 50/50 blend fires both).
const NOTIFY_MIN_WEIGHT: f32 = 0.5 - 1e-4;

/// Evaluate one clip instance: advance time, sample, gather notifies / root motion / morphs.
fn eval_clip(
    clip: &AnimationClip,
    st: &mut ClipState,
    ctx: &mut EvalContext,
    out: &mut Pose,
    weight: Weight,
    play: ClipPlay,
) -> Result<NodeOut, EvalError> {
    let step = match play.sync {
        // Synced: driven from outside on a shared normalised clock.
        Some(phase) => ClipStep {
            prev: phase.prev * clip.duration,
            time: phase.time * clip.duration,
            loops: phase.loops,
        },
        None => {
            let delta = ctx.delta * play.speed;
            // A clip that starts playing in reverse starts from its end.
            let from = if !st.started && delta < 0.0 {
                clip.duration
            } else {
                st.time
            };
            clip.step(from, delta, play.looping)
        }
    };
    let include_start = !st.started;
    st.started = true;
    st.time = step.time;
    let reverse = step.is_reverse() || ctx.delta * play.speed < 0.0;
    st.finished = !play.looping
        && if reverse {
            step.time <= 1e-5
        } else {
            step.time >= clip.duration - 1e-5
        };

    // The pose must be fully defined: reset to bind, then sample on top.
    out.copy_from(&ctx.bind);
    clip.sample_pose(step.time, out);

    // Notifies crossed by this step, in playback order. A clip that adds nothing to the pose
    // (the far side of a blend at 0, a cross-fade source at its end) stays quiet, so a blend
    // pinned to one input does not fire the other input's footsteps too.
    if play.notifies {
        let first = ctx.notifies.len();
        clip.notifies_in_step(&step, include_start, &mut ctx.notifies);
        ctx.notify_log
            .extend(ctx.notifies[first..].iter().map(|n| n.name.clone()));
        // Only clips that dominate the blend are heard: a sample under half the effective weight
        // would otherwise trigger footsteps and hit windows from a few percent of the pose.
        if weight.audible < NOTIFY_MIN_WEIGHT {
            ctx.notifies.truncate(first);
        }
    }

    // Root motion: the effective mode (an enclosing RootMotion node may override the clip's
    // import-time mode). Parents blend it with the weights they blend the pose with.
    let mode = ctx.rm_mode_override.unwrap_or_else(|| {
        clip.root_motion
            .as_ref()
            .map_or(RootMotionMode::None, |rm| rm.mode)
    });
    let mut root = RootDelta::ZERO;
    if mode != RootMotionMode::None
        && let Some((translation, rotation)) = clip.root_motion_delta_step(&step)
    {
        let (translation, rotation) = filter_root_motion(mode, translation, rotation);
        root = RootDelta {
            translation,
            rotation,
        };
        clip.pin_root_in_place(out, mode);
    }

    // Morph weights, key-sorted for the parents' blends (two tracks for one target: the first
    // wins).
    let mut shared = ctx.morph_pools.take_shared();
    clip.sample_morphs(step.time, &mut shared);
    shared.sort_by_key(|m| m.0);
    shared.dedup_by_key(|m| m.0);
    let mut nodes = ctx.morph_pools.take_nodes();
    let mut raw = std::mem::take(&mut ctx.node_raw);
    if clip.has_node_morphs() {
        clip.sample_node_morphs(step.time, &mut raw);
        nodes.clear();
        nodes.extend(
            raw.iter()
                .copied()
                .map(|(node, target, value)| ((node, target), value)),
        );
        nodes.sort_by_key(|m| m.0);
        nodes.dedup_by_key(|m| m.0);
    } else {
        nodes.clear();
    }
    ctx.node_raw = raw;
    let morphs = MorphMix { shared, nodes };
    let weight_pose = weight.pose;

    // Named curves, weighted like the pose. The sample buffer keeps its name strings.
    let mut curves = std::mem::take(&mut ctx.curve_sample);
    clip.sample_curves(step.time, &mut curves);
    for (name, value) in &curves {
        ctx.record_curve(name, *value, weight_pose);
    }
    ctx.curve_sample = curves;
    Ok(NodeOut { root, morphs })
}

/// Blend-space evaluation shared by the 1D and 2D nodes: the active samples (weight > 0) are
/// evaluated and combined with their weights, which sum to 1.
///
/// Synced spaces share one normalized clock. It advances at the weighted average of the active
/// samples' durations, so the cadence changes smoothly as the weights move, and it continues
/// from the dominant sample's phase. Every active sample steps from that shared phase, so a
/// sample that sat idle at weight 0 does not replay the time it missed (no root-motion spike
/// when it fades in), and only the dominant sample fires notifies (no doubled footsteps).
fn eval_blendspace<'clip>(
    clip_at: impl Fn(usize) -> &'clip str,
    sample_count: usize,
    runtime: &mut BlendRuntime,
    sync: Option<&str>,
    ctx: &mut EvalContext,
    out: &mut Pose,
    weight: Weight,
) -> Result<NodeOut, EvalError> {
    if sample_count == 0 || runtime.states.is_empty() {
        return Ok(NodeOut::ZERO);
    }
    // Resolve every sample before any clock advances, so a missing clip leaves the space
    // where it was.
    for i in 0..sample_count {
        let name = clip_at(i);
        if ctx.clips.get(name).is_none() {
            return Err(EvalError::MissingClip(name.to_string()));
        }
    }
    runtime.clips.clear();
    for i in 0..sample_count {
        let name = clip_at(i);
        if let Some(clip) = ctx.clips.get(name) {
            runtime.clips.push(Arc::clone(clip));
        }
    }
    let BlendRuntime {
        states,
        weights,
        active,
        clips,
        ..
    } = runtime;
    // The slot after the samples keeps the shared clock. An instance with any other layout (built
    // for another revision of the space) has no usable clock and falls back to the dominant
    // sample's.
    let split_at = sample_count.min(states.len());
    let (states, rest) = states.split_at_mut(split_at);
    if states.is_empty() {
        return Ok(NodeOut::ZERO);
    }
    let clock: Option<&mut ClipState> = if rest.len() == 1 { rest.first_mut() } else { None };
    // An instance built for another revision of the space may have fewer states: skip
    // samples it cannot hold rather than index out of range.
    active.clear();
    active.extend(
        weights
            .iter()
            .copied()
            .filter(|&(i, w)| w > 1e-6 && i < states.len() && i < clips.len()),
    );
    let dominant = active
        .iter()
        .copied()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);

    // A declared named group puts the space's shared clock under the group's leader (ADR-1465).
    let setup = ctx.sync_setup;
    let named = sync.filter(|g| setup.is_some_and(|s| s.declares(g))).zip(setup);
    let mut cycle = 0.0f32;
    let mut follower = false;
    let shared = if sync.is_some() {
        dominant.and_then(|d| {
            let total: f32 = active.iter().map(|&(_, w)| w).sum();
            let duration = active
                .iter()
                .map(|&(i, w)| w * crate::num::finite_or(clips[i].duration, 0.0).max(0.0))
                .sum::<f32>()
                / total.max(1e-6);
            let dominant_duration = clips[d].duration;
            if !duration.is_finite()
                || duration <= 0.0
                || !dominant_duration.is_finite()
                || dominant_duration <= 0.0
            {
                return None;
            }
            // The stored clock survives the parameter jumping to samples that sat idle (a
            // sprint pressed from a walk): their own clocks are stale, the shared one is not.
            // End the clock borrow before reading the sample slice. The two come from one
            // split, and a match that uses both in different arms still holds the clock.
            let stored_phase = match clock.as_deref() {
                Some(stored) if stored.started => Some(stored.time),
                _ => None,
            };
            let phase = stored_phase.unwrap_or_else(|| states[d].time / dominant_duration);
            cycle = duration;
            let own = ClipStep::advance(phase.clamp(0.0, 1.0), ctx.delta / duration, 1.0, true);
            let Some((group, setup)) = named else {
                return Some(own);
            };
            let query = SyncQuery {
                member: clip_at(d),
                duration,
                own_time: phase.clamp(0.0, 1.0) * duration,
                delta: ctx.delta,
                markers: false,
            };
            Some(match ctx.sync.play(setup, group, query) {
                SyncPlay::Own => own,
                SyncPlay::Lead(step) => normalised(step, duration),
                SyncPlay::Follow(step) => {
                    follower = true;
                    normalised(step, duration)
                }
            })
        })
    } else {
        None
    };
    if let (Some(step), Some(stored)) = (shared, clock) {
        stored.time = step.time;
        stored.started = true;
    }

    // Incremental weighted blend: after k samples, `out` holds Σ wᵢ·poseᵢ / Σ wᵢ over the
    // active set; blending the next sample with w/(Σ + w) keeps the result exact. Root motion
    // follows the same weights.
    let mut root = NodeOut::ZERO;
    let mut accumulated_weight = 0.0f32;
    let mut out_valid = false;
    let mut tmp = ctx.take_pose(ctx.skeleton.len());
    for &(index, w) in active.iter() {
        let play = ClipPlay {
            looping: true,
            speed: 1.0,
            sync: shared,
            // A synced space hears only its dominant sample (ADR-871), also when the shared
            // clock could not be built (a zero-length dominant clip): the fallback to own
            // clocks must not switch it to the unsynced every-sample rule (deep-zai-animation
            // finding 4).
            notifies: (sync.is_none() || Some(index) == dominant) && !follower,
        };
        let sample_root = eval_clip(&clips[index], &mut states[index], ctx, &mut tmp, weight * w, play)?;
        if !out_valid {
            out.copy_from(&tmp);
            root = sample_root;
            out_valid = true;
            accumulated_weight = w;
        } else {
            let blend_t = w / (accumulated_weight + w);
            out.blend_from(&tmp, blend_t);
            root = root.lerp(sample_root, blend_t, &mut ctx.morph_pools);
            accumulated_weight += w;
        }
    }
    if !out_valid {
        // Defensive: no usable weights (they sum to 1 by construction) — play sample 0.
        root = eval_clip(
            &clips[0],
            &mut states[0],
            ctx,
            out,
            weight,
            ClipPlay::own(true, 1.0),
        )?;
    }
    ctx.give_pose(tmp);
    if let (Some((group, setup)), Some(step), Some(d)) = (named, shared, dominant) {
        ctx.sync.record(
            setup,
            group,
            SyncMember {
                clip: clip_at(d),
                weight: weight.pose,
                time: step.time * cycle,
                speed: 1.0,
                looping: true,
                duration: cycle,
            },
        );
    }
    Ok(root)
}

/// State-machine evaluation.
fn eval_state_machine(
    def: &StateMachineDef,
    sm: &mut StateMachineInstance,
    ctx: &mut EvalContext,
    out: &mut Pose,
    weight: Weight,
) -> Result<NodeOut, EvalError> {
    if sm.current >= sm.states.len() || sm.current >= def.states.len() {
        return Ok(NodeOut::ZERO);
    }
    // A reload can leave an in-flight transition pointing past the new definition's lists:
    // drop it rather than index out of range.
    if sm.transition.as_ref().is_some_and(|t| {
        t.def >= def.transitions.len() || t.from >= sm.states.len() || t.from >= def.states.len()
    }) {
        sm.transition = None;
    }
    if !sm.entered_initial {
        sm.entered_initial = true;
        ctx.state_events.push(StateEvent {
            from: None,
            to: def.states[sm.current].0.clone(),
        });
    }

    // 1. Progress an active transition. A cross-fade's alpha weights both states this frame:
    //    the target by alpha, the source by 1 − alpha (poses, root motion and morphs alike).
    let blend_dt = ctx.blend_dt();
    if let Some(transition) = sm.transition.as_mut() {
        transition.elapsed += blend_dt;
    }
    let crossfade = sm
        .transition
        .as_ref()
        .filter(|t| !t.blend.is_inertial())
        .and_then(|t| {
            let def_t = def.transitions.get(t.def)?;
            let progress = (t.elapsed / def_t.duration.max(1e-6)).clamp(0.0, 1.0);
            Some((t.from, unit_weight(def_t.curve.alpha(progress))))
        });
    let current_weight = crossfade.map_or(1.0, |(_, alpha)| alpha);

    // 2. Evaluate the current state; snapshot the notifies it crosses this frame (audible or
    //    not: a machine inside a branch at weight 0 still follows its `Notify` conditions).
    let notify_mark = ctx.notify_log.len();
    let (current_name, current_node) = &def.states[sm.current];
    let mut root = eval_into(
        current_node,
        &mut sm.states[sm.current],
        ctx,
        out,
        weight * current_weight,
    )?;
    let finished = sm.states[sm.current].any_clip_finished();
    let fired: Vec<String> = ctx.notify_log[notify_mark..].to_vec();

    // 3. A cross-fade evaluates its source state and blends it under the target pose with
    //    (1 − alpha); an inertialized one evaluates nothing more. A self-transition restarted
    //    the one instance both would share, so it has no separate source to blend.
    if let Some((from, alpha)) = crossfade
        && from != sm.current
        && from < sm.states.len()
        && from < def.states.len()
    {
        let (_, from_node) = &def.states[from];
        let mut tmp = ctx.take_pose(out.len());
        let from_root = eval_into(
            from_node,
            &mut sm.states[from],
            ctx,
            &mut tmp,
            weight * (1.0 - alpha),
        )?;
        out.blend_from(&tmp, 1.0 - alpha);
        root = root.lerp(from_root, 1.0 - alpha, &mut ctx.morph_pools);
        ctx.give_pose(tmp);
    }

    // 3b. The decaying inertial offset goes on top of whatever the base pose is (the target
    //     state, or a cross-fade that interrupted an inertialized transition).
    if sm.inertia.is_active() {
        sm.inertia.advance(blend_dt);
        sm.inertia.apply(out);
    }

    // 4. Fire transitions. During a blend, only when the running transition is interruptible.
    let blending = sm.transition.is_some();
    let can_interrupt = sm
        .transition
        .as_ref()
        .is_none_or(|t| def.transitions.get(t.def).is_some_and(|d| d.interruptible));
    if !blending || can_interrupt {
        for (ti, transition_def) in def.transitions.iter().enumerate() {
            let source_ok = transition_def
                .from
                .as_ref()
                .is_none_or(|from| from == current_name);
            if !source_ok {
                continue;
            }
            let Some(target) = def.state_index(&transition_def.to) else {
                continue;
            };
            // An any-state transition never re-enters the state that is already current: a
            // condition that stays true (`IsDead`) would otherwise restart it every frame. An
            // explicit self-transition (`from: Some(X), to: X`) does restart X.
            if transition_def.from.is_none() && target == sm.current {
                continue;
            }
            if target >= sm.states.len() {
                continue;
            }
            if !def.conditions_hold(&transition_def.conditions, ctx.params, finished, &fired) {
                continue;
            }
            // The target's evaluation below can fail (a missing clip): undo the trigger,
            // state-event and current-state changes so gameplay never sees a half-entered state.
            let rollback = (
                ctx.state_events.len(),
                ctx.consumed_triggers.len(),
                sm.transition.clone(),
                sm.current,
            );
            // Record trigger consumption; the system applies it after evaluation.
            for condition in &transition_def.conditions {
                if let Condition::Trigger { param } = condition {
                    ctx.consumed_triggers.push(param.clone());
                }
            }
            ctx.state_events.push(StateEvent {
                from: Some(current_name.clone()),
                to: transition_def.to.clone(),
            });
            let blend = transition_def.blend;
            // A cross-fade's fire frame already counts toward its duration; an inertial decay
            // starts at the current output (elapsed 0) and is `delta` in on the next frame.
            sm.transition = Some(ActiveTransition {
                def: ti,
                from: sm.current,
                elapsed: if blend.is_inertial() { 0.0 } else { blend_dt },
                blend,
            });
            sm.current = target;
            let (_, target_node) = &def.states[target];
            // Entering a state starts it from the beginning: a second jump plays the jump
            // again instead of resuming the finished clip.
            GraphInstance::reset(target_node, &mut sm.states[target]);
            let mut tmp = ctx.take_pose(out.len());
            match blend.decay() {
                None => {
                    // Advance the target's clock so it is in step; at alpha 0 it contributes
                    // nothing to the pose, root motion or morphs yet.
                    // Its notifies are heard, as loud as the machine: a notify at the start of
                    // the entered state.
                    if let Err(error) = eval_into(
                        target_node,
                        &mut sm.states[target],
                        ctx,
                        &mut tmp,
                        weight.silent_pose(),
                    ) {
                        ctx.give_pose(tmp);
                        ctx.state_events.truncate(rollback.0);
                        ctx.consumed_triggers.truncate(rollback.1);
                        sm.transition = rollback.2;
                        sm.current = rollback.3;
                        return Err(error);
                    }
                }
                Some(method) => {
                    // Inertialize: the target's pose one frame ago (side-effect-free probe of a
                    // copy of its instance, at delta 0) and now (advancing its clock).
                    let mut target_prev = ctx.take_pose(out.len());
                    let probed = probe_pose(target_node, &sm.states[target], ctx, &mut target_prev)
                        .and_then(|()| eval_into(target_node, &mut sm.states[target], ctx, &mut tmp, weight));
                    if let Err(error) = probed {
                        ctx.give_pose(target_prev);
                        ctx.give_pose(tmp);
                        ctx.state_events.truncate(rollback.0);
                        ctx.consumed_triggers.truncate(rollback.1);
                        sm.transition = rollback.2;
                        sm.current = rollback.3;
                        return Err(error);
                    }
                    sm.tracks_history = true;
                    sm.inertia.start(
                        method,
                        transition_def.duration,
                        Switch {
                            source: &*out,
                            source_prev: sm.history_valid.then_some(&sm.prev_output),
                            target: &tmp,
                            target_prev: Some(&target_prev),
                            dt: blend_dt,
                        },
                    );
                    // `out` already is the source pose = target ⊕ offset at elapsed 0; a
                    // zero-length inertialization is a cut.
                    if !sm.inertia.is_active() {
                        out.copy_from(&tmp);
                    }
                    ctx.give_pose(target_prev);
                }
            }
            // The fire frame still shows the source pose, so it keeps the source's root motion.
            ctx.give_pose(tmp);
            break;
        }
    }

    // 5. Complete a finished transition.
    let complete = sm
        .transition
        .as_ref()
        .is_some_and(|t| def.transitions.get(t.def).is_none_or(|d| t.elapsed >= d.duration));
    if complete {
        sm.transition = None;
        ctx.state_events.push(StateEvent {
            from: None,
            to: def.states[sm.current].0.clone(),
        });
    }

    // 6. Remember this frame's output for the next inertialization's source velocity.
    if sm.tracks_history {
        sm.prev_output.copy_from(out);
        sm.history_valid = true;
    }
    Ok(root)
}

/// Evaluate `node` from a copy of `instance` at delta 0 (the pose at its current clock, one
/// frame before the real evaluation advances it), with every side effect on `ctx` undone:
/// notifies, curves, state events and consumed triggers are restored, its root motion is
/// discarded, and the zero weight keeps morph sums unchanged.
fn probe_pose(
    node: &AnimNode,
    instance: &GraphInstance,
    ctx: &mut EvalContext,
    out: &mut Pose,
) -> Result<(), EvalError> {
    let mut copy = instance.clone();
    let marks = (
        ctx.notifies.len(),
        ctx.state_events.len(),
        ctx.curves.len(),
        ctx.consumed_triggers.len(),
        ctx.notify_log.len(),
    );
    // Sync groups too (audit v033b): the probe's members and its delta-0 steps must not elect a
    // leader or freeze the group's clock for the real evaluation.
    let sync_mark = ctx.sync.mark();
    let curve_values: Vec<f32> = ctx.curves.iter().map(|(_, value)| *value).collect();
    let curve_weight_values = ctx.curve_weights.clone();
    let delta = ctx.delta;
    ctx.delta = 0.0;
    let result = eval_into(node, &mut copy, ctx, out, Weight::ZERO);
    ctx.delta = delta;
    ctx.sync.rewind(&sync_mark);
    ctx.notifies.truncate(marks.0);
    ctx.state_events.truncate(marks.1);
    ctx.curves.truncate(marks.2);
    ctx.curve_weights.truncate(marks.2);
    for (slot, value) in ctx.curves.iter_mut().zip(curve_values) {
        slot.1 = value;
    }
    for (slot, value) in ctx.curve_weights.iter_mut().zip(curve_weight_values) {
        *slot = value;
    }
    ctx.consumed_triggers.truncate(marks.3);
    ctx.notify_log.truncate(marks.4);
    result.map(|_| ())
}

/// Most distinct warnings `first_warning` remembers; past it, new keys stay silent so rigs that
/// churn (spawn and despawn) cannot grow the set without bound.
const WARNED_CAP: usize = 1024;

/// True the first time `key` is seen in this process (one warning per rig and socket, IK limit or
/// warp parameter, instead of one per frame). Bounded by `WARNED_CAP`. Property tracks keep their
/// own per-world set (`property::PropertyWarnings`, ADR-10131).
pub(crate) fn first_warning(key: String) -> bool {
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let Ok(mut guard) = WARNED.lock() else {
        return false;
    };
    let set = guard.get_or_insert_with(Default::default);
    set.len() < WARNED_CAP && set.insert(key)
}

/// The object-space position an IK node aims at, or `None` when it cannot be resolved this
/// frame (a non-finite parameter, or a socket that has not been placed: the node leaves the pose
/// alone, with one warning per rig and socket name, rather than pulling the limb to the origin).
fn resolve_target(target: &IkTarget, ctx: &EvalContext) -> Option<Vec3> {
    let position = match target {
        IkTarget::Param(name) => ctx.params.vec3(name),
        IkTarget::Socket(name) => {
            let found = ctx.socket_targets.get(&(ctx.rig, name.clone())).copied();
            if found.is_none() && first_warning(format!("ik socket {:?} {name}", ctx.rig)) {
                tracing::warn!(
                    target: "regia_animation",
                    "IK socket {name:?} is not resolved on {:?}; the node leaves the pose unchanged",
                    ctx.rig
                );
            }
            found?
        }
    };
    position.is_finite().then_some(position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::{JointTrack, Notify, RootMotionTrack, TrackCurve};

    #[test]
    fn first_warning_fires_once_per_key() {
        let key = format!("first-warning-test {}", std::process::id());
        assert!(first_warning(key.clone()));
        assert!(!first_warning(key.clone()));
        assert!(!first_warning(key));
    }
    use crate::graph::{
        AdditiveNode, BlendNode, ClipNode, LayerEntry, LayerNode, RootMotionNode, TwoBoneIKNode,
    };
    use crate::pose::translated;
    use crate::skeleton::LocalBind;
    use crate::state_machine::TransitionCurve;
    use crate::state_machine::{Condition, TransitionDef};

    fn skeleton2() -> Skeleton {
        let names: Vec<String> = ["root", "child"].iter().map(|s| s.to_string()).collect();
        Skeleton::from_local_bind(
            names,
            vec![None, Some(0)],
            vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("valid")
    }

    fn slide_clip(name: &str, dx: f32, duration: f32) -> AnimationClip {
        let mut clip = AnimationClip::empty(name, duration);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, duration],
                values: vec![Vec3::ZERO, Vec3::new(dx, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip
    }

    type Setup = (
        ClipLibrary,
        ParameterSet,
        Vec<Pose>,
        HashMap<(Entity, String), Vec3>,
    );

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn setup(_skeleton: &Skeleton, clips: Vec<AnimationClip>) -> Setup {
        let mut lib = ClipLibrary::new();
        for clip in clips {
            lib.insert(clip);
        }
        (lib, ParameterSet::new(), Vec::new(), HashMap::new())
    }

    #[test]
    fn clip_node_samples_and_loops() {
        let sk = skeleton2();
        let (lib, params, scratch, sockets) = setup(&sk, vec![slide_clip("Slide", 2.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "Slide".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.5, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let scratch = ctx.finish().scratch;
        assert!((out.translations[0].x - 1.0).abs() < 1e-5);

        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.75, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        // 0.5 + 0.75 wraps to 0.25 into a 1 s clip.
        assert!((out.translations[0].x - 0.5).abs() < 1e-5);
    }

    #[test]
    fn missing_clip_reports_error() {
        let sk = skeleton2();
        let (lib, params, scratch, sockets) = setup(&sk, vec![]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "Nope".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        let err = evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect_err("missing clip");
        assert!(matches!(err, EvalError::MissingClip(name) if name == "Nope"));
    }

    #[test]
    fn blend_crossfades_by_param() {
        let sk = skeleton2();
        let (lib, mut params, scratch, sockets) =
            setup(&sk, vec![slide_clip("A", 1.0, 1.0), slide_clip("B", 3.0, 1.0)]);
        params.set_float("w", 0.25);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(AnimNode::Clip(ClipNode {
                    clip: "A".into(),
                    looping: false,
                    speed: 1.0,
                    sync_group: None,
                })),
                b: Box::new(AnimNode::Clip(ClipNode {
                    clip: "B".into(),
                    looping: false,
                    speed: 1.0,
                    sync_group: None,
                })),
                alpha_param: "w".into(),
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 1.0, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        // Both clips ended their slide (duration 1): pose = A*(0.75) + B*(0.25).
        assert!((out.translations[0].x - (1.0 * 0.75 + 3.0 * 0.25)).abs() < 1e-5);
    }

    #[test]
    fn notifies_reach_output() {
        let sk = skeleton2();
        let mut clip = slide_clip("Step", 0.0, 1.0);
        clip.notifies.push(Notify {
            time: 0.5,
            name: "Footstep".into(),
            payload: None,
        });
        let (lib, params, scratch, sockets) = setup(&sk, vec![clip]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "Step".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.5, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        assert!(ctx.notifies.iter().any(|n| n.name == "Footstep"));
        let scratch = ctx.finish().scratch;
        // And not again before the next wrap.
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.4, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let no_notify = !ctx.notifies.iter().any(|n| n.name == "Footstep");
        ctx.finish();
        assert!(no_notify);
    }

    #[test]
    fn state_machine_transitions_on_trigger_with_timing() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Idle", 0.0, 1.0));
        lib.insert(slide_clip("Jump", 2.0, 1.0));
        let mut params = ParameterSet::new();
        let sm = StateMachineDef {
            initial: "Grounded".into(),
            states: vec![
                (
                    "Grounded".into(),
                    AnimNode::Clip(ClipNode {
                        clip: "Idle".into(),
                        looping: true,
                        speed: 1.0,
                        sync_group: None,
                    }),
                ),
                (
                    "Jump".into(),
                    AnimNode::Clip(ClipNode {
                        clip: "Jump".into(),
                        looping: true,
                        speed: 1.0,
                        sync_group: None,
                    }),
                ),
            ],
            transitions: vec![TransitionDef {
                from: None,
                to: "Jump".into(),
                conditions: vec![Condition::Trigger { param: "Jump".into() }],
                duration: 0.5,
                curve: TransitionCurve::Linear,
                interruptible: false,
                blend: Default::default(),
            }],
        };
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(sm),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let sockets = HashMap::new();
        let scratch = Vec::new();
        let mut out = Pose::from_identity(sk.len());

        // Frame 1: initial enter event.
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let events = ctx.state_events.clone();
        let scratch = ctx.finish().scratch;
        assert_eq!(
            events,
            vec![StateEvent {
                from: None,
                to: "Grounded".into()
            }]
        );

        // Frame 2: fire the trigger; the transition begins. On the fire frame the blend
        // alpha is 0, so the pose is still fully Grounded (x = 0).
        params.set_trigger("Jump");
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.25, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let events = ctx.state_events.clone();
        let scratch = ctx.finish().scratch;
        assert!(
            events
                .iter()
                .any(|e| e.from.as_deref() == Some("Grounded") && e.to == "Jump")
        );
        assert!(
            (out.translations[0].x - 0.0).abs() < 1e-5,
            "x = {}",
            out.translations[0].x
        );

        // Frame 3: elapsed = 0.5 = duration, alpha = 1: the pose is fully Jump. Jump has
        // advanced 0.5 s of its 1 s slide to 2 m → x = 1.0, and the completed-enter event
        // fires.
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.25, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let events = ctx.state_events.clone();
        let _ = ctx.finish();
        assert!(
            events.iter().any(|e| e.to == "Jump" && e.from.is_none()),
            "expected completed-enter event, got {events:?}"
        );
        assert!(
            (out.translations[0].x - 1.0).abs() < 1e-4,
            "x = {}",
            out.translations[0].x
        );
    }

    #[test]
    fn state_machine_drops_transition_missing_from_reloaded_graph() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Idle", 0.0, 1.0));
        lib.insert(slide_clip("Jump", 2.0, 1.0));
        let mut params = ParameterSet::new();
        let state = |name: &str, clip: &str| {
            (
                name.to_string(),
                AnimNode::Clip(ClipNode {
                    clip: clip.into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                }),
            )
        };
        let mut sm = StateMachineDef {
            initial: "Grounded".into(),
            states: vec![state("Grounded", "Idle"), state("Jump", "Jump")],
            transitions: vec![TransitionDef {
                from: None,
                to: "Jump".into(),
                conditions: vec![Condition::Trigger { param: "Jump".into() }],
                duration: 5.0,
                curve: TransitionCurve::Linear,
                interruptible: false,
                blend: Default::default(),
            }],
        };
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(sm.clone()),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let sockets = HashMap::new();
        let mut out = Pose::from_identity(sk.len());
        params.set_trigger("Jump");
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let scratch = ctx.finish().scratch;
        let GraphInstance::StateMachine(running) = &inst else {
            panic!("state machine instance")
        };
        assert!(running.transition.is_some(), "transition in flight");

        // Hot reload removes every transition while one is playing: no panic, transition dropped.
        sm.transitions.clear();
        let reloaded = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(sm),
        };
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&reloaded, &mut inst, &mut ctx, &mut out).expect("eval after reload");
        let GraphInstance::StateMachine(running) = &inst else {
            panic!("state machine instance")
        };
        assert!(running.transition.is_none());
    }

    #[test]
    fn a_fresh_blend_space_instance_matches_its_node() {
        // `from_node` adds the shared clock slot; `matches_node` must agree, or
        // `evaluate_graph` rebuilds the space every frame and its clocks never move.
        let node = space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], true);
        assert!(GraphInstance::from_node(&node).matches_node(&node));
        let other = space_1d("Speed", &[(0.0, "Walk"), (0.5, "Jog"), (1.0, "Run")], true);
        assert!(!GraphInstance::from_node(&node).matches_node(&other));
    }

    #[test]
    fn evaluate_graph_rebuilds_instance_of_a_different_shape() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Slide", 2.0, 1.0));
        let params = ParameterSet::new();
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "Slide".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        // An instance left over from a state-machine root: it used to write nothing.
        let mut inst = GraphInstance::BlendSpace1D(BlendRuntime::from_count(3));
        assert!(!inst.matches_node(&graph.root));
        let sockets = HashMap::new();
        let mut out = Pose::from_identity(sk.len());
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.5, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        assert!(inst.matches_node(&graph.root));
        assert!(
            (out.translations[0].x - 1.0).abs() < 1e-4,
            "x = {}",
            out.translations[0].x
        );
    }

    #[test]
    fn failed_transition_target_leaves_no_side_effects() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Idle", 0.0, 1.0)); // "Missing" is deliberately not loaded
        let mut params = ParameterSet::new();
        let state = |name: &str, clip: &str| {
            (
                name.to_string(),
                AnimNode::Clip(ClipNode {
                    clip: clip.into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                }),
            )
        };
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Grounded".into(),
                states: vec![state("Grounded", "Idle"), state("Jump", "Missing")],
                transitions: vec![TransitionDef {
                    from: None,
                    to: "Jump".into(),
                    conditions: vec![Condition::Trigger { param: "Jump".into() }],
                    duration: 0.5,
                    curve: TransitionCurve::Linear,
                    interruptible: false,
                    blend: Default::default(),
                }],
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let sockets = HashMap::new();
        let mut out = Pose::from_identity(sk.len());
        params.set_trigger("Jump");
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        let result = evaluate_graph(&graph, &mut inst, &mut ctx, &mut out);
        assert!(matches!(result, Err(EvalError::MissingClip(_))));
        // Only the initial-enter event survives; the trigger stays set and the machine stays put.
        assert_eq!(ctx.state_events.len(), 1, "{:?}", ctx.state_events);
        assert!(ctx.state_events[0].from.is_none());
        assert!(ctx.consumed_triggers.is_empty());
        let GraphInstance::StateMachine(sm) = &inst else {
            panic!("state machine instance")
        };
        assert_eq!(sm.current, 0);
        assert!(sm.transition.is_none());
    }

    #[test]
    fn layer_masks_upper_body() {
        // 3 joints; base moves all, layer moves only joint 2 ("arm").
        let names: Vec<String> = ["root", "arm"].iter().map(|s| s.to_string()).collect();
        let sk = Skeleton::from_local_bind(
            names,
            vec![None, Some(0)],
            vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("valid");
        let mut base_clip = AnimationClip::empty("Base", 1.0);
        base_clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(1.0, 1.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut layer_clip = AnimationClip::empty("Wave", 1.0);
        layer_clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(5.0, 1.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let (lib, mut params, scratch, sockets) = setup(&sk, vec![base_clip, layer_clip]);
        params.set_float("WaveWeight", 1.0);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Base".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                layers: vec![LayerEntry {
                    graph: Box::new(AnimNode::Clip(ClipNode {
                        clip: "Wave".into(),
                        looping: true,
                        speed: 1.0,
                        sync_group: None,
                    })),
                    mask: crate::graph::JointMaskDef {
                        joints: vec!["arm".into()],
                        include_children: false,
                    },
                    mode: LayerMode::Override,
                    weight_param: "WaveWeight".into(),
                    weight_inertialize: 0.0,
                }],
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        assert_eq!(
            out.translations[1],
            Vec3::new(5.0, 1.0, 0.0),
            "masked joint overridden"
        );
    }

    #[test]
    fn root_motion_override_mode() {
        let sk = skeleton2();
        let mut clip = slide_clip("Walk", 2.0, 1.0);
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XYZ,
        });
        let (lib, params, scratch, sockets) = setup(&sk, vec![clip]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::RootMotion(RootMotionNode {
                inner: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Walk".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                mode: RootMotionMode::XZ,
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.5, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        let root_motion = ctx.root_motion;
        ctx.finish();
        assert!((root_motion.0.x - 1.0).abs() < 1e-5);
        // XZ extraction leaves the rig in place on X while the delta carries the motion.
        assert!(
            out.translations[0].x.abs() < 1e-4,
            "in-place x {}",
            out.translations[0].x
        );
    }

    #[test]
    fn additive_layer_applies_deltas() {
        let sk = skeleton2();
        let mut base = AnimationClip::empty("Base", 1.0);
        base.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::new(1.0, 0.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut add = AnimationClip::empty("Lean", 1.0);
        add.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::new(0.0, 0.5, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let (lib, mut params, scratch, sockets) = setup(&sk, vec![base, add]);
        params.set_float("AddWeight", 1.0);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Additive(AdditiveNode {
                base: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Base".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                additive: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Lean".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                weight_param: "AddWeight".into(),
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        assert!(out.translations[0].abs_diff_eq(Vec3::new(1.0, 0.5, 0.0), 1e-5));
        // The child joint is not in the additive clip, so it stays at its bind translation
        // instead of stacking the bind pose on itself.
        assert!(
            (out.translations[1].y - 1.0).abs() < 1e-4,
            "bind stacked onto itself: {}",
            out.translations[1]
        );
    }

    #[test]
    fn missing_ik_socket_leaves_pose_unchanged() {
        let names: Vec<String> = ["Shoulder", "Elbow", "Hand"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let sk = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1)],
            vec![
                translated(Vec3::new(0.0, 1.0, 0.0)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
            ],
        )
        .expect("valid");
        let idle = AnimationClip::empty("Idle", 1.0);
        let (lib, params, scratch, sockets) = setup(&sk, vec![idle]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Idle".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                chain: ["Shoulder".into(), "Elbow".into(), "Hand".into()],
                target: IkTarget::Socket("NoSuchSocket".into()),
                pole: None,
                weight_param: None,
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        // Without the fix the chain was pulled toward the origin; now it keeps its bind pose.
        let globals = sk.globals(&out, None);
        assert!((globals[2].w_axis.truncate() - Vec3::new(0.0, 1.0, -0.8)).length() < 1e-4);
    }

    #[test]
    fn two_bone_ik_via_graph() {
        // Chain: shoulder → elbow → hand, all −Z.
        let names: Vec<String> = ["Shoulder", "Elbow", "Hand"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let sk = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1)],
            vec![
                translated(Vec3::new(0.0, 1.0, 0.0)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
            ],
        )
        .expect("valid");
        let idle = AnimationClip::empty("Idle", 1.0);
        let (lib, mut params, scratch, sockets) = setup(&sk, vec![idle]);
        let target = Vec3::new(0.3, 1.1, -0.5);
        params.set_vec3("ReachTarget", target);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Idle".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                chain: ["Shoulder".into(), "Elbow".into(), "Hand".into()],
                target: IkTarget::Param("ReachTarget".into()),
                pole: Some("Pole".into()),
                weight_param: None,
            }),
        };
        params.set_vec3("Pole", Vec3::new(0.0, 5.0, 0.0));
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        let globals = sk.globals(&out, None);
        let tip = globals[2].w_axis.truncate();
        assert!(
            (tip - target).length() < 1e-3,
            "tip {tip} vs {target} ({})",
            (tip - target).length()
        );
    }

    // --- Robustness (ADR-871) -----------------------------------------------------------

    /// What one evaluated frame produced.
    struct Frame {
        root: Vec3,
        notifies: Vec<String>,
        events: Vec<StateEvent>,
        curves: Vec<(String, f32)>,
        morphs: Vec<(u32, f32)>,
    }

    fn frame(
        graph: &AnimGraph,
        inst: &mut GraphInstance,
        sk: &Skeleton,
        lib: &ClipLibrary,
        params: &ParameterSet,
        dt: f32,
        out: &mut Pose,
    ) -> Frame {
        let sockets = HashMap::new();
        let mut ctx = EvalContext::new(sk, lib, params, dt, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(graph, inst, &mut ctx, out).expect("eval");
        let root = ctx.root_motion.0;
        let notifies = ctx.notifies.iter().map(|n| n.name.clone()).collect();
        let events = ctx.state_events.clone();
        let curves = ctx.curves.clone();
        let morphs = ctx.finish().morphs;
        Frame {
            root,
            notifies,
            events,
            curves,
            morphs,
        }
    }

    /// A looping clip whose root slides along +X at `speed` m/s, extracted as XZ root motion.
    fn walker(name: &str, speed: f32, duration: f32) -> AnimationClip {
        let mut clip = slide_clip(name, speed * duration, duration);
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        clip
    }

    fn clip_node(clip: &str, looping: bool, speed: f32) -> AnimNode {
        AnimNode::Clip(ClipNode {
            clip: clip.into(),
            looping,
            speed,
            sync_group: None,
        })
    }

    fn space_1d(param: &str, samples: &[(f32, &str)], sync: bool) -> AnimNode {
        AnimNode::BlendSpace1D(crate::graph::BlendSpace1DNode {
            space: crate::blendspace::BlendSpace1D {
                param: param.into(),
                samples: samples
                    .iter()
                    .map(|&(value, clip)| crate::blendspace::BlendSample1D {
                        value,
                        clip: clip.into(),
                    })
                    .collect(),
            },
            sync_group: sync.then(|| "gait".to_string()),
        })
    }

    fn transition(from: Option<&str>, to: &str, conditions: Vec<Condition>, duration: f32) -> TransitionDef {
        TransitionDef {
            from: from.map(str::to_string),
            to: to.into(),
            conditions,
            duration,
            curve: TransitionCurve::Linear,
            interruptible: false,
            blend: TransitionBlend::Crossfade,
        }
    }

    fn library(clips: Vec<AnimationClip>) -> ClipLibrary {
        let mut lib = ClipLibrary::new();
        for clip in clips {
            lib.insert(clip);
        }
        lib
    }

    #[test]
    fn blend_space_root_motion_is_weighted_not_winner_take_all() {
        let sk = skeleton2();
        let lib = library(vec![walker("Walk", 1.0, 1.0), walker("Run", 3.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], false),
        };
        let root_at = |speed: f32| {
            let mut params = ParameterSet::new();
            params.set_float("Speed", speed);
            let mut inst = GraphInstance::from_node(&graph.root);
            let mut out = Pose::from_identity(sk.len());
            frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out).root.x
        };
        // Halfway: the average of 0.1 m and 0.3 m. The old rule took one sample's motion.
        assert!((root_at(0.5) - 0.2).abs() < 1e-4, "{}", root_at(0.5));
        // Continuous through the 50 % point, where the old winner flipped from walk to run.
        assert!((root_at(0.51) - root_at(0.49)).abs() < 0.01);
    }

    #[test]
    fn crossfade_weights_root_motion_and_morphs_by_alpha() {
        let sk = skeleton2();
        let mut idle = slide_clip("Idle", 0.0, 10.0);
        idle.morph_tracks
            .push(crate::clip::MorphTrack::new(0, TrackCurve::Static(1.0)));
        let mut walk = walker("Walk", 2.0, 10.0);
        walk.morph_tracks
            .push(crate::clip::MorphTrack::new(0, TrackCurve::Static(0.0)));
        let lib = library(vec![idle, walk]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    ("Walk".into(), clip_node("Walk", true, 1.0)),
                ],
                transitions: vec![transition(
                    Some("Idle"),
                    "Walk",
                    vec![Condition::Bool {
                        param: "Go".into(),
                        value: true,
                    }],
                    1.0,
                )],
            }),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        params.set_bool("Go", true);
        let fire = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert!(
            fire.root.length() < 1e-6,
            "the fire frame still shows Idle: {}",
            fire.root
        );
        // Elapsed 0.2 of 1 s: Walk weighs 0.2. It moved 0.2 m, so the rig moves 0.04 m; the
        // old rule handed the full 0.2 m to Walk because Idle has no root motion.
        let mid = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert!((mid.root.x - 0.04).abs() < 1e-4, "{}", mid.root);
        // Morphs follow the same weights: 0.8 · 1 + 0.2 · 0, not the old 50/50 average.
        let morph = mid.morphs.iter().find(|m| m.0 == 0).map(|m| m.1);
        assert!(morph.is_some_and(|m| (m - 0.8).abs() < 1e-4), "{morph:?}");
    }

    fn jump_graph() -> AnimGraph {
        AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Grounded".into(),
                states: vec![
                    ("Grounded".into(), clip_node("Idle", true, 1.0)),
                    ("Jump".into(), clip_node("Jump", false, 1.0)),
                ],
                transitions: vec![
                    transition(
                        None,
                        "Jump",
                        vec![Condition::Trigger { param: "Jump".into() }],
                        0.0,
                    ),
                    transition(Some("Jump"), "Grounded", vec![Condition::ClipFinished], 0.0),
                ],
            }),
        }
    }

    #[test]
    fn reentering_a_state_restarts_it() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("Idle", 0.0, 1.0), slide_clip("Jump", 2.0, 0.5)]);
        let graph = jump_graph();
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut landed = 0;
        for jump in 0..2 {
            params.set_trigger("Jump");
            frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            params.consume_trigger("Jump");
            // One frame into the jump: 0.2 s of 0.5 s along a 2 m slide.
            let next = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            assert!(
                (out.translations[0].x - 0.8).abs() < 1e-4,
                "jump {jump}: x {} (a resumed, finished clip shows 2.0)",
                out.translations[0].x
            );
            assert!(
                next.events.iter().all(|e| e.to != "Grounded"),
                "jump {jump} lands at once"
            );
            for _ in 0..6 {
                let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
                landed += f
                    .events
                    .iter()
                    .filter(|e| e.from.as_deref() == Some("Jump"))
                    .count();
            }
        }
        assert_eq!(landed, 2, "each jump plays to the end and lands once");
    }

    #[test]
    fn any_state_transition_does_not_restart_the_current_state() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("Idle", 0.0, 1.0), slide_clip("Dead", 1.0, 1.0)]);
        let mut to_dead = transition(
            None,
            "Dead",
            vec![Condition::Bool {
                param: "IsDead".into(),
                value: true,
            }],
            0.2,
        );
        to_dead.interruptible = true;
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    ("Dead".into(), clip_node("Dead", false, 1.0)),
                ],
                transitions: vec![to_dead],
            }),
        };
        let mut params = ParameterSet::new();
        params.set_bool("IsDead", true);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut switches = 0;
        for _ in 0..10 {
            let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            switches += f.events.iter().filter(|e| e.from.is_some()).count();
        }
        assert_eq!(switches, 1, "a held condition enters Dead once");
        // Dead kept playing to the end of its 1 s clip (the fire frame counts), instead of
        // restarting every frame.
        assert!(
            (out.translations[0].x - 1.0).abs() < 1e-4,
            "{}",
            out.translations[0].x
        );
    }

    #[test]
    fn reverse_playback_plays_backwards_and_finishes_at_the_start() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("S", 2.0, 1.0)]);
        let params = ParameterSet::new();
        for looping in [false, true] {
            let graph = AnimGraph {
                parameters: vec![],
                root: clip_node("S", looping, -1.0),
            };
            let mut inst = GraphInstance::from_node(&graph.root);
            let mut out = Pose::from_identity(sk.len());
            let mut xs = Vec::new();
            for _ in 0..5 {
                frame(&graph, &mut inst, &sk, &lib, &params, 0.25, &mut out);
                xs.push(out.translations[0].x);
            }
            let finished = matches!(&inst, GraphInstance::Clip(st) if st.finished);
            let expected = if looping {
                [1.5, 1.0, 0.5, 0.0, 1.5]
            } else {
                [1.5, 1.0, 0.5, 0.0, 0.0]
            };
            for (x, e) in xs.iter().zip(expected) {
                assert!((x - e).abs() < 1e-4, "looping {looping}: {xs:?}");
            }
            assert_eq!(finished, !looping, "a reverse one-shot finishes at its start");
        }
    }

    fn arm() -> Skeleton {
        let names: Vec<String> = ["Shoulder", "Elbow", "Hand"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1)],
            vec![
                translated(Vec3::new(0.0, 1.0, 0.0)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
                translated(Vec3::new(0.0, 0.0, -0.4)),
            ],
        )
        .expect("valid")
    }

    #[test]
    fn ik_with_an_unresolved_socket_leaves_the_pose_alone() {
        let sk = arm();
        let lib = library(vec![AnimationClip::empty("Idle", 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(clip_node("Idle", true, 1.0)),
                chain: ["Shoulder".into(), "Elbow".into(), "Hand".into()],
                target: IkTarget::Socket("not_placed_yet".into()),
                pole: None,
                weight_param: None,
            }),
        };
        let params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // The old code aimed the hand at the rig origin while the socket was missing.
        assert_eq!(out, sk.bind_pose_pose());
    }

    #[test]
    fn non_finite_parameters_never_reach_the_pose() {
        let sk = arm();
        // Track-less clips: the pose is the bind pose, so any non-finite value is the blend's.
        let lib = library(vec![
            AnimationClip::empty("A", 1.0),
            AnimationClip::empty("B", 1.0),
        ]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(AnimNode::Blend(BlendNode {
                    a: Box::new(clip_node("A", true, 1.0)),
                    b: Box::new(space_1d("Speed", &[(0.0, "A"), (1.0, "B")], true)),
                    alpha_param: "W".into(),
                })),
                chain: ["Shoulder".into(), "Elbow".into(), "Hand".into()],
                target: IkTarget::Param("Target".into()),
                pole: Some("Pole".into()),
                weight_param: Some("IkWeight".into()),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", f32::NAN);
        params.set_float("Speed", f32::NAN);
        params.set_float("IkWeight", f32::INFINITY);
        params.set_vec3("Target", Vec3::new(f32::NAN, 1.0, 0.0));
        params.set_vec3("Pole", Vec3::splat(f32::INFINITY));
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        for _ in 0..3 {
            let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            assert!(f.root.is_finite());
        }
        for i in 0..out.len() {
            assert!(
                out.translations[i].is_finite(),
                "joint {i}: {}",
                out.translations[i]
            );
            assert!(out.rotations[i].is_finite(), "joint {i}: {:?}", out.rotations[i]);
            assert!(out.scales[i].is_finite());
        }
        // A finite target still works through the same node.
        params.set_vec3("Target", Vec3::new(0.3, 1.1, -0.5));
        params.set_float("IkWeight", 1.0);
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        let tip = sk.globals(&out, None)[2].w_axis.truncate();
        assert!((tip - Vec3::new(0.3, 1.1, -0.5)).length() < 1e-3, "{tip}");
    }

    /// Walk (1 m/s over 1 s) and run (3 m/s over 0.5 s), each with a `Step` notify half way.
    fn gait_library() -> ClipLibrary {
        let mut walk = walker("Walk", 1.0, 1.0);
        walk.notifies.push(Notify {
            time: 0.5,
            name: "Step".into(),
            payload: None,
        });
        let mut run = walker("Run", 3.0, 0.5);
        run.notifies.push(Notify {
            time: 0.25,
            name: "Step".into(),
            payload: None,
        });
        library(vec![walk, run])
    }

    #[test]
    fn synced_blend_space_root_motion_is_smooth_while_a_sample_fades_in() {
        let sk = skeleton2();
        let lib = gait_library();
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], true),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let dt = 1.0 / 60.0;
        let mut roots = Vec::new();
        for k in 0..=120 {
            params.set_float("Speed", k as f32 / 120.0);
            roots.push(frame(&graph, &mut inst, &sk, &lib, &params, dt, &mut out).root.x);
        }
        let worst = roots
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        // The speed ramps smoothly from 1 m/s to 3 m/s. A winner-take-all pick jumps by about
        // 0.033 m per frame where run overtakes walk; a sample fading in from a stale clock
        // replays the time it sat idle.
        assert!(worst < 0.005, "largest frame-to-frame root motion change {worst}");
        assert!(roots.iter().all(|r| *r > 0.0));
        let first = roots[1] / dt;
        let last = roots[120] / dt;
        assert!(
            (first - 1.0).abs() < 0.05 && (last - 3.0).abs() < 0.05,
            "{first} → {last} m/s"
        );
    }

    #[test]
    fn synced_blend_space_fires_only_the_dominant_samples_notifies() {
        let sk = skeleton2();
        let lib = gait_library();
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], true),
        };
        let mut params = ParameterSet::new();
        params.set_float("Speed", 0.5);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut steps = 0;
        for _ in 0..120 {
            let f = frame(&graph, &mut inst, &sk, &lib, &params, 1.0 / 60.0, &mut out);
            steps += f.notifies.iter().filter(|n| *n == "Step").count();
        }
        // 2 s at a 0.75 s blended lap crosses the half-lap point three times; both samples
        // firing would double every footstep.
        assert_eq!(steps, 3);
    }

    #[test]
    fn curves_are_weighted_like_the_pose() {
        let sk = skeleton2();
        let mut a = slide_clip("A", 0.0, 1.0);
        a.curves.push(crate::clip::NamedCurve {
            name: "FootPlant".into(),
            curve: TrackCurve::Static(0.0),
        });
        let mut b = slide_clip("B", 0.0, 1.0);
        b.curves.push(crate::clip::NamedCurve {
            name: "FootPlant".into(),
            curve: TrackCurve::Static(1.0),
        });
        let lib = library(vec![a, b]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(clip_node("A", true, 1.0)),
                b: Box::new(clip_node("B", true, 1.0)),
                alpha_param: "W".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", 0.25);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // One entry, the weighted value (the old list held both and the last one won).
        assert_eq!(f.curves.len(), 1, "{:?}", f.curves);
        assert!((f.curves[0].1 - 0.25).abs() < 1e-5, "{:?}", f.curves);
    }

    fn clip_with_notify(name: &str, notify: &str, at: f32) -> AnimationClip {
        let mut clip = slide_clip(name, 0.0, 1.0);
        clip.notifies.push(Notify {
            time: at,
            name: notify.into(),
            payload: None,
        });
        clip
    }

    #[test]
    fn a_blend_pinned_to_one_input_fires_only_that_inputs_notifies() {
        let sk = skeleton2();
        let lib = library(vec![
            clip_with_notify("A", "a_step", 0.05),
            clip_with_notify("B", "b_step", 0.05),
        ]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(clip_node("A", true, 1.0)),
                b: Box::new(clip_node("B", true, 1.0)),
                alpha_param: "W".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", 0.0);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // B adds nothing to the pose at W = 0, so its footstep is not heard.
        assert_eq!(f.notifies, ["a_step"]);
    }

    #[test]
    fn a_notify_at_the_start_of_an_entered_state_fires() {
        let sk = skeleton2();
        let lib = library(vec![
            slide_clip("Idle", 0.0, 1.0),
            clip_with_notify("Attack", "swing", 0.0),
        ]);
        let mut attack = transition(
            None,
            "Attack",
            vec![Condition::Trigger { param: "Go".into() }],
            0.2,
        );
        attack.interruptible = true;
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    ("Attack".into(), clip_node("Attack", false, 1.0)),
                ],
                transitions: vec![attack],
            }),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        params.set_trigger("Go");
        let fire = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // The entered clip starts at 0, where its notify sits: it fires on the fire frame even
        // though the cross-fade target weighs 0 there. The old (0, t] interval skipped it.
        assert_eq!(fire.notifies, ["swing"]);
        params.consume_trigger("Go");
        let next = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert!(next.notifies.is_empty(), "fires once: {:?}", next.notifies);
    }

    #[test]
    fn notify_conditions_still_follow_a_branch_at_zero_weight() {
        let sk = skeleton2();
        // S2 has a notify at its very start: entering S2 inside the hidden branch must not be
        // heard either (a fire-frame flag once unmuted the entered state regardless).
        let lib = library(vec![
            clip_with_notify("S1", "go", 0.05),
            clip_with_notify("S2", "enter", 0.0),
            slide_clip("Other", 0.0, 1.0),
        ]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(AnimNode::StateMachine(StateMachineDef {
                    initial: "S1".into(),
                    states: vec![
                        ("S1".into(), clip_node("S1", true, 1.0)),
                        ("S2".into(), clip_node("S2", true, 1.0)),
                    ],
                    transitions: vec![transition(
                        Some("S1"),
                        "S2",
                        vec![Condition::Notify("go".into())],
                        0.0,
                    )],
                })),
                b: Box::new(clip_node("Other", true, 1.0)),
                alpha_param: "W".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", 1.0);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // The machine weighs 0 (W = 1 shows only `Other`) but still follows its notify...
        assert!(
            f.events
                .iter()
                .any(|e| e.from.as_deref() == Some("S1") && e.to == "S2"),
            "{:?}",
            f.events
        );
        // ...which nobody hears.
        assert!(f.notifies.is_empty(), "{:?}", f.notifies);
    }

    /// Brief items (b) and (c) of the root-motion lane (ADR-2604; both already fixed on this
    /// base, this pins them together): the synced clock is one normalized clock at the
    /// weighted cycle length (0.5·1 s + 0.5·0.5 s = 0.75 s), and a long step that laps it
    /// several times keeps every lap's root motion. Walk adds 1 m and run 1.5 m per lap, so
    /// the 50/50 blend travels 1.25 m per 0.75 s: 2 s cover 2⅔ laps, 3⅓ m. A clock at the
    /// dominant sample's rate, or one counting a single wrap, misses that.
    #[test]
    fn a_synced_space_counts_every_lap_of_its_shared_clock() {
        let sk = skeleton2();
        let lib = gait_library();
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], true),
        };
        let mut params = ParameterSet::new();
        params.set_float("Speed", 0.5);
        let mut out = Pose::from_identity(sk.len());
        // In one long step and in 120 short ones, the same distance.
        let mut inst = GraphInstance::from_node(&graph.root);
        let hitch = frame(&graph, &mut inst, &sk, &lib, &params, 2.0, &mut out).root.x;
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut smooth = 0.0;
        for _ in 0..120 {
            smooth += frame(&graph, &mut inst, &sk, &lib, &params, 2.0 / 120.0, &mut out)
                .root
                .x;
        }
        let expected = 1.25 * 2.0 / 0.75;
        assert!(
            (hitch - expected).abs() < 1e-3,
            "one 2 s step: {hitch} m, expected {expected}"
        );
        assert!(
            (smooth - expected).abs() < 1e-3,
            "120 steps: {smooth} m, expected {expected}"
        );
    }

    #[test]
    fn a_synced_space_keeps_its_phase_when_the_parameter_jumps() {
        let sk = skeleton2();
        let lib = gait_library();
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], true),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let dt = 1.0 / 60.0;
        for _ in 0..20 {
            frame(&graph, &mut inst, &sk, &lib, &params, dt, &mut out);
        }
        // Sprint pressed: the speed snaps from walk-only to run-only. Run has never played; it
        // must pick up the walk's phase (same foot forward), not start its own clock at 0.
        params.set_float("Speed", 1.0);
        frame(&graph, &mut inst, &sk, &lib, &params, dt, &mut out);
        let GraphInstance::BlendSpace1D(runtime) = &inst else {
            panic!("blend space instance");
        };
        let walk_phase = runtime.states[0].time / 1.0;
        let run_phase = runtime.states[1].time / 0.5;
        assert!((walk_phase - 20.0 / 60.0).abs() < 1e-4, "walk {walk_phase}");
        assert!(
            (run_phase - (walk_phase + dt / 0.5)).abs() < 1e-4,
            "run phase {run_phase} should continue the walk's {walk_phase}"
        );
    }

    #[test]
    fn layer_root_motion_follows_how_much_of_the_root_the_mask_covers() {
        let sk = skeleton2();
        let lib = library(vec![walker("Walk", 1.0, 1.0), walker("Run", 3.0, 1.0)]);
        let layered = |joints: &[&str]| AnimGraph {
            parameters: vec![],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(clip_node("Walk", true, 1.0)),
                layers: vec![LayerEntry {
                    graph: Box::new(clip_node("Run", true, 1.0)),
                    mask: crate::graph::JointMaskDef {
                        joints: joints.iter().map(|j| j.to_string()).collect(),
                        include_children: true,
                    },
                    mode: LayerMode::Override,
                    weight_param: "W".into(),
                    weight_inertialize: 0.0,
                }],
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", 0.5);
        let root_with = |graph: &AnimGraph| {
            let mut inst = GraphInstance::from_node(&graph.root);
            let mut out = Pose::from_identity(sk.len());
            frame(graph, &mut inst, &sk, &lib, &params, 0.1, &mut out).root.x
        };
        // A full-body layer at 0.5: half walk (0.1 m), half run (0.3 m).
        let full = root_with(&layered(&["root"]));
        assert!((full - 0.2).abs() < 1e-4, "{full}");
        // An upper-body layer does not touch the root: the base's motion alone.
        let upper = root_with(&layered(&["child"]));
        assert!((upper - 0.1).abs() < 1e-4, "{upper}");
    }

    #[test]
    fn additive_root_motion_adds_on_top_of_the_base() {
        let sk = skeleton2();
        let lib = library(vec![walker("Walk", 1.0, 1.0), walker("Drift", 0.5, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Additive(AdditiveNode {
                base: Box::new(clip_node("Walk", true, 1.0)),
                additive: Box::new(clip_node("Drift", true, 1.0)),
                weight_param: "A".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("A", 0.5);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        // 0.1 m from the base plus half of the additive's 0.05 m.
        assert!((f.root.x - 0.125).abs() < 1e-4, "{}", f.root);
    }

    #[test]
    fn entering_a_state_fires_only_the_start_notifies_it_shows() {
        // The entered state is a blend pinned to A: A's start notify is heard on the fire frame,
        // B's (weight 0 inside the state) is not.
        let sk = skeleton2();
        let lib = library(vec![
            slide_clip("Idle", 0.0, 1.0),
            clip_with_notify("A", "a_start", 0.0),
            clip_with_notify("B", "b_start", 0.0),
        ]);
        let mut go = transition(None, "Act", vec![Condition::Trigger { param: "Go".into() }], 0.3);
        go.interruptible = true;
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    (
                        "Act".into(),
                        AnimNode::Blend(BlendNode {
                            a: Box::new(clip_node("A", false, 1.0)),
                            b: Box::new(clip_node("B", false, 1.0)),
                            alpha_param: "W".into(),
                        }),
                    ),
                ],
                transitions: vec![go],
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("W", 0.0);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        params.set_trigger("Go");
        let fire = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert_eq!(fire.notifies, ["a_start"]);
    }

    #[test]
    fn entering_a_state_under_half_weight_fires_nothing() {
        // The machine is the light side of a blend (alpha 0.4). Enter keeps the machine's
        // audible weight and drops the start notify, the same gate as any other clip.
        let sk = skeleton2();
        let lib = library(vec![
            slide_clip("Hold", 0.0, 1.0),
            slide_clip("Idle", 0.0, 1.0),
            clip_with_notify("A", "a_start", 0.0),
        ]);
        let mut go = transition(None, "Act", vec![Condition::Trigger { param: "Go".into() }], 0.3);
        go.interruptible = true;
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(clip_node("Hold", true, 1.0)),
                b: Box::new(AnimNode::StateMachine(StateMachineDef {
                    initial: "Idle".into(),
                    states: vec![
                        ("Idle".into(), clip_node("Idle", true, 1.0)),
                        ("Act".into(), clip_node("A", false, 1.0)),
                    ],
                    transitions: vec![go],
                })),
                alpha_param: "Alpha".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("Alpha", 0.4);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        params.set_trigger("Go");
        let fire = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert!(
            fire.notifies.is_empty(),
            "enter at weight 0.4 stays silent: {:?}",
            fire.notifies
        );
    }

    fn with_smile(mut clip: AnimationClip, smile: f32) -> AnimationClip {
        clip.morph_tracks
            .push(crate::clip::MorphTrack::new(3, TrackCurve::Static(smile)));
        clip
    }

    fn smile_of(f: &Frame) -> Option<f32> {
        f.morphs.iter().find(|m| m.0 == 3).map(|m| m.1)
    }

    #[test]
    fn a_morph_only_one_side_has_fades_through_a_crossfade() {
        // Smiling idle → walk without face tracks over 1 s. The smile fades with the cross-fade;
        // per-target normalisation held it at 1 for the whole fade and then dropped it.
        let sk = skeleton2();
        let lib = library(vec![
            with_smile(slide_clip("Idle", 0.0, 10.0), 1.0),
            walker("Walk", 1.0, 10.0),
        ]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    ("Walk".into(), clip_node("Walk", true, 1.0)),
                ],
                transitions: vec![transition(
                    Some("Idle"),
                    "Walk",
                    vec![Condition::Bool {
                        param: "Go".into(),
                        value: true,
                    }],
                    1.0,
                )],
            }),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let start = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert_eq!(smile_of(&start), Some(1.0));
        params.set_bool("Go", true);
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        let mut smiles = Vec::new();
        for _ in 0..9 {
            let f = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            smiles.push(smile_of(&f).unwrap_or(0.0));
        }
        // Alpha 0.2 … 1.0 in steps of 0.1: the smile falls 0.8 … 0.0, never jumping.
        for (k, smile) in smiles.iter().enumerate() {
            let alpha = 0.2 + 0.1 * k as f32;
            assert!((smile - (1.0 - alpha)).abs() < 1e-4, "frame {k}: {smiles:?}");
        }
        let after = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert_eq!(smile_of(&after).unwrap_or(0.0), 0.0);
    }

    #[test]
    fn layers_and_additives_weight_their_morphs() {
        let sk = skeleton2();
        let lib = library(vec![
            slide_clip("Body", 0.0, 1.0),
            with_smile(slide_clip("BodySmiling", 0.0, 1.0), 0.2),
            with_smile(slide_clip("Face", 0.0, 1.0), 1.0),
            with_smile(slide_clip("Grin", 0.0, 1.0), 0.5),
        ]);
        let mut params = ParameterSet::new();
        params.set_float("W", 0.5);
        params.set_float("A", 1.0);
        let run = |graph: &AnimGraph| {
            let mut inst = GraphInstance::from_node(&graph.root);
            let mut out = Pose::from_identity(sk.len());
            smile_of(&frame(graph, &mut inst, &sk, &lib, &params, 0.1, &mut out))
        };
        // A face layer at 0.5 over a body without face tracks: half a smile (it was the full
        // smile whatever the layer weight).
        let layered = AnimGraph {
            parameters: vec![],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(clip_node("Body", true, 1.0)),
                layers: vec![LayerEntry {
                    graph: Box::new(clip_node("Face", true, 1.0)),
                    mask: crate::graph::JointMaskDef {
                        joints: vec![],
                        include_children: false,
                    },
                    mode: LayerMode::Override,
                    weight_param: "W".into(),
                    weight_inertialize: 0.0,
                }],
            }),
        };
        let smile = run(&layered);
        assert!(smile.is_some_and(|v| (v - 0.5).abs() < 1e-5), "{smile:?}");
        // An additive grin adds to the base's smile (it was averaged with it).
        let additive = AnimGraph {
            parameters: vec![],
            root: AnimNode::Additive(AdditiveNode {
                base: Box::new(clip_node("BodySmiling", true, 1.0)),
                additive: Box::new(clip_node("Grin", true, 1.0)),
                weight_param: "A".into(),
            }),
        };
        let smile = run(&additive);
        assert!(smile.is_some_and(|v| (v - 0.7).abs() < 1e-5), "{smile:?}");
    }

    #[test]
    fn a_nan_layer_easing_time_does_not_poison_the_pose() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("Base", 1.0, 1.0), slide_clip("Over", 3.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(clip_node("Base", true, 1.0)),
                layers: vec![LayerEntry {
                    graph: Box::new(clip_node("Over", true, 1.0)),
                    mask: crate::graph::JointMaskDef {
                        joints: vec!["root".into()],
                        include_children: true,
                    },
                    mode: LayerMode::Override,
                    weight_param: "W".into(),
                    weight_inertialize: f32::NAN,
                }],
            }),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        for k in 0..4 {
            params.set_float("W", if k < 2 { 0.0 } else { 1.0 });
            frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            assert!(
                out.translations.iter().all(|t| t.is_finite()),
                "frame {k}: {:?}",
                out.translations
            );
        }
        // No easing: the layer applies at once. Its clip only advances while it weighs more than
        // 0, so after two weighted frames it is 0.2 s into its 3 m/s slide.
        assert!(
            (out.translations[0].x - 0.6).abs() < 1e-4,
            "{}",
            out.translations[0]
        );
    }

    // --- Fix-side regression tests (integ/v0129) -------------------------------------------

    fn step(
        sk: &Skeleton,
        lib: &ClipLibrary,
        params: &ParameterSet,
        graph: &AnimGraph,
        inst: &mut GraphInstance,
        delta: f32,
    ) -> (Pose, Vec<Notify>, Vec<String>) {
        let sockets = HashMap::new();
        let mut ctx = EvalContext::new(sk, lib, params, delta, Entity::PLACEHOLDER, &sockets, Vec::new());
        let mut out = Pose::from_identity(sk.len());
        evaluate_graph(graph, inst, &mut ctx, &mut out).expect("eval");
        let notifies = ctx.notifies.clone();
        let consumed = ctx.consumed_triggers.clone();
        ctx.finish();
        (out, notifies, consumed)
    }

    #[test]
    fn one_shot_state_replays_after_reentry() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Idle", 0.0, 1.0));
        lib.insert(slide_clip("Attack", 2.0, 1.0));
        let go = |to: &str, from: &str| TransitionDef {
            from: Some(from.into()),
            to: to.into(),
            conditions: vec![if to == "Attack" {
                Condition::Trigger { param: "Go".into() }
            } else {
                Condition::ClipFinished
            }],
            duration: 0.1,
            curve: TransitionCurve::Linear,
            interruptible: false,
            blend: Default::default(),
        };
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    ("Idle".into(), clip_node("Idle", true, 1.0)),
                    ("Attack".into(), clip_node("Attack", false, 1.0)),
                ],
                transitions: vec![go("Attack", "Idle"), go("Idle", "Attack")],
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut params = ParameterSet::new();
        step(&sk, &lib, &params, &graph, &mut inst, 0.1);
        // First attack: fire, then run it to its end so ClipFinished returns to Idle.
        params.set_trigger("Go");
        let (_, _, consumed) = step(&sk, &lib, &params, &graph, &mut inst, 0.1);
        assert_eq!(consumed, vec!["Go".to_string()]);
        params.consume_trigger("Go");
        step(&sk, &lib, &params, &graph, &mut inst, 1.0);
        step(&sk, &lib, &params, &graph, &mut inst, 0.2);
        // Second attack must start from the beginning, not hold the last frame. The fire frame
        // still shows Idle (ADR-871) and starts Attack's clock (0.1 s; the 0.1 s cross-fade is
        // complete); the next frame shows Attack 0.2 s in.
        params.set_trigger("Go");
        let (fire, _, _) = step(&sk, &lib, &params, &graph, &mut inst, 0.1);
        assert!(
            fire.translations[0].x.abs() < 1e-4,
            "fire frame x = {}",
            fire.translations[0].x
        );
        params.consume_trigger("Go");
        let (out, _, _) = step(&sk, &lib, &params, &graph, &mut inst, 0.1);
        assert!(
            (out.translations[0].x - 0.4).abs() < 1e-4,
            "x = {} (2.0 means the one-shot kept its finished state)",
            out.translations[0].x
        );
    }

    #[test]
    fn any_clip_finished_blend_ignores_looping_partner() {
        let finished = GraphInstance::Clip(ClipState {
            time: 1.0,
            finished: true,
            started: true,
        });
        let looping = GraphInstance::Clip(ClipState::default());
        let blend = GraphInstance::Blend {
            a: Box::new(looping.clone()),
            b: Box::new(finished.clone()),
        };
        assert!(blend.any_clip_finished());
        let additive = GraphInstance::Additive {
            base: Box::new(looping.clone()),
            additive: Box::new(finished),
        };
        assert!(additive.any_clip_finished());
        let none = GraphInstance::Blend {
            a: Box::new(looping.clone()),
            b: Box::new(looping),
        };
        assert!(!none.any_clip_finished());
    }

    fn notify_clip(name: &str, notify: &str) -> AnimationClip {
        let mut clip = slide_clip(name, 1.0, 1.0);
        clip.notifies = vec![Notify {
            time: 0.5,
            name: notify.into(),
            payload: None,
        }];
        clip
    }

    #[test]
    fn blended_clips_fire_notifies_only_when_dominant() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(notify_clip("A", "foot_a"));
        lib.insert(notify_clip("B", "foot_b"));
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Blend(BlendNode {
                a: Box::new(clip_node("A", true, 1.0)),
                b: Box::new(clip_node("B", true, 1.0)),
                alpha_param: "Alpha".into(),
            }),
        };
        let names = |alpha: f32| {
            let mut params = ParameterSet::new();
            params.set_float("Alpha", alpha);
            let mut inst = GraphInstance::from_node(&graph.root);
            let (_, notifies, _) = step(&sk, &lib, &params, &graph, &mut inst, 0.6);
            notifies.into_iter().map(|n| n.name).collect::<Vec<_>>()
        };
        assert_eq!(names(0.1), ["foot_a"]);
        assert_eq!(names(0.4), ["foot_a"], "the 0.4 side is under half weight");
        assert_eq!(names(0.9), ["foot_b"]);
        assert_eq!(names(0.5), ["foot_a", "foot_b"]);
    }

    #[test]
    fn a_synced_blend_space_under_half_weight_fires_nothing() {
        let sk = skeleton2();
        let lib = library(vec![
            notify_clip("A", "foot_a"),
            notify_clip("B", "foot_b"),
            notify_clip("C", "foot_c"),
        ]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::BlendSpace2D(crate::graph::BlendSpace2DNode {
                space: crate::blendspace::BlendSpace2D {
                    param_x: "X".into(),
                    param_y: "Y".into(),
                    samples: vec![
                        crate::blendspace::BlendSample2D {
                            point: [0.0, 0.0],
                            clip: "A".into(),
                        },
                        crate::blendspace::BlendSample2D {
                            point: [1.0, 0.0],
                            clip: "B".into(),
                        },
                        crate::blendspace::BlendSample2D {
                            point: [0.0, 1.0],
                            clip: "C".into(),
                        },
                    ],
                    triangles: vec![[0, 1, 2]],
                },
                sync_group: Some("gait".into()),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("X", 1.0 / 3.0);
        params.set_float("Y", 1.0 / 3.0);
        let weights = match &graph.root {
            AnimNode::BlendSpace2D(space) => space.space.weights([1.0 / 3.0, 1.0 / 3.0]),
            _ => panic!("blend space"),
        };
        let dominant = weights.iter().map(|(_, w)| *w).fold(0.0f32, f32::max);
        assert!(
            dominant < 0.5 - 1e-4,
            "the centroid must leave every sample under half, got {weights:?}"
        );
        let mut inst = GraphInstance::from_node(&graph.root);
        let (_, notifies, _) = step(&sk, &lib, &params, &graph, &mut inst, 0.6);
        assert!(
            notifies.is_empty(),
            "a dominant sample under half is silent: {notifies:?}"
        );
    }

    #[test]
    fn stale_transition_after_graph_swap_is_dropped() {
        let sk = skeleton2();
        let mut lib = ClipLibrary::new();
        lib.insert(slide_clip("Idle", 0.0, 1.0));
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![("Idle".into(), clip_node("Idle", true, 1.0))],
                transitions: vec![],
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        if let GraphInstance::StateMachine(sm) = &mut inst {
            sm.transition = Some(ActiveTransition {
                def: 7,
                from: 5,
                elapsed: 0.0,
                blend: Default::default(),
            });
        }
        let params = ParameterSet::new();
        step(&sk, &lib, &params, &graph, &mut inst, 0.1);
        let GraphInstance::StateMachine(sm) = &inst else {
            panic!("state machine instance");
        };
        assert!(sm.transition.is_none());
    }

    #[test]
    fn additive_delta_matches_delta_from_and_reuses_scratch() {
        let sk = skeleton2();
        let mut base = AnimationClip::empty("Base", 1.0);
        base.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut add = AnimationClip::empty("Lean", 1.0);
        add.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::new(0.0, 0.5, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let (lib, mut params, mut pool, sockets) = setup(&sk, vec![base, add]);
        params.set_float("AddWeight", 1.0);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Additive(AdditiveNode {
                base: Box::new(clip_node("Base", true, 1.0)),
                additive: Box::new(clip_node("Lean", true, 1.0)),
                weight_param: "AddWeight".into(),
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut caps = None;
        for step in 0..4 {
            let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, pool);
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            let finished = ctx.finish();
            pool = finished.scratch;
            if step == 1 {
                assert!(pool.len() >= 2, "an additive eval keeps the sample and the delta");
                for pose in &pool {
                    assert!(pose.translations.capacity() >= sk.len());
                }
                caps = Some(
                    pool.iter()
                        .map(|pose| pose.translations.capacity())
                        .collect::<Vec<_>>(),
                );
            }
        }
        let later = pool
            .iter()
            .map(|pose| pose.translations.capacity())
            .collect::<Vec<_>>();
        assert_eq!(later, caps.expect("step 2"));
        // t = 0.4 on the base slide (1 m over 1 s) plus the lean's delta from bind.
        let mut sampled = sk.bind_pose_pose();
        lib.get("Lean").expect("lean").sample_pose(0.0, &mut sampled);
        let mut delta = Pose::default();
        sampled.delta_into(&sk.bind_pose_pose(), &mut delta);
        assert_eq!(
            delta.translations[0],
            sampled.delta_from(&sk.bind_pose_pose()).translations[0]
        );
        let mut manual = sk.bind_pose_pose();
        lib.get("Base").expect("base").sample_pose(0.4, &mut manual);
        manual.additive_from(&delta, 1.0);
        assert!(out.translations[0].abs_diff_eq(manual.translations[0], 1e-5));
        assert!((out.translations[1].y - 1.0).abs() < 1e-4);

        let additive_time = {
            let GraphInstance::Additive { additive, .. } = &inst else {
                panic!("additive instance");
            };
            let GraphInstance::Clip(state) = additive.as_ref() else {
                panic!("additive clip");
            };
            state.time
        };
        params.set_float("AddWeight", 0.0);
        for _ in 0..5 {
            let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, pool);
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            pool = ctx.finish().scratch;
        }
        let GraphInstance::Additive { base, additive } = &inst else {
            panic!("additive instance");
        };
        let GraphInstance::Clip(add_state) = additive.as_ref() else {
            panic!("additive clip");
        };
        let GraphInstance::Clip(base_state) = base.as_ref() else {
            panic!("base clip");
        };
        assert_eq!(add_state.time.to_bits(), additive_time.to_bits());
        assert!(
            (base_state.time - 0.9).abs() < 1e-4,
            "base keeps advancing: {}",
            base_state.time
        );
        // Weight 0 leaves the lean off. The base has looped: 0.9 s of a 1 m slide.
        assert!(
            (out.translations[0].x - 0.9).abs() < 1e-4,
            "{}",
            out.translations[0]
        );
        assert!(out.translations[0].y.abs() < 1e-4);
    }

    #[test]
    fn layer_mask_weights_match_resolve_and_stay_cached() {
        let names: Vec<String> = ["root", "arm"].iter().map(|s| s.to_string()).collect();
        let sk = Skeleton::from_local_bind(
            names,
            vec![None, Some(0)],
            vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("valid");
        let mask_def = crate::graph::JointMaskDef {
            joints: vec!["arm".into()],
            include_children: false,
        };
        let expected = mask_def.resolve(&sk.joint_names).weights(&sk.parents, sk.len());
        let (lib, mut params, _, sockets) = setup(
            &sk,
            vec![slide_clip("Base", 1.0, 1.0), slide_clip("Wave", 2.0, 1.0)],
        );
        params.set_float("WaveWeight", 1.0);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(clip_node("Base", true, 1.0)),
                layers: vec![LayerEntry {
                    graph: Box::new(clip_node("Wave", true, 1.0)),
                    mask: mask_def,
                    mode: LayerMode::Override,
                    weight_param: "WaveWeight".into(),
                    weight_inertialize: 0.0,
                }],
            }),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        let (cap, joints) = {
            let GraphInstance::Layer {
                mask_weights,
                mask_joints,
                ..
            } = &inst
            else {
                panic!("layer");
            };
            let AnimNode::Layer(def) = &graph.root else {
                panic!("layer node");
            };
            assert_eq!(*mask_joints, layer_mask_key(&sk, &def.layers));
            assert_eq!(mask_weights[0], expected);
            assert!(mask_weights[0].capacity() >= sk.len());
            (mask_weights[0].capacity(), *mask_joints)
        };
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        let GraphInstance::Layer {
            mask_weights,
            mask_joints,
            ..
        } = &inst
        else {
            panic!("layer");
        };
        assert_eq!(mask_weights[0], expected);
        assert_eq!(mask_weights[0].capacity(), cap);
        assert_eq!(*mask_joints, joints);

        // Audit (corr-animation): the cache was keyed on the joint count alone, so an edited
        // mask on a kept instance (graph hot reload, editor preview) stayed the old one.
        let mut edited = graph.clone();
        let AnimNode::Layer(def) = &mut edited.root else {
            panic!("layer node");
        };
        def.layers[0].mask.joints = vec!["root".into()];
        let edited_mask = def.layers[0].mask.clone();
        let edited_expected = edited_mask
            .resolve(&sk.joint_names)
            .weights(&sk.parents, sk.len());
        assert_ne!(edited_expected, expected);
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        evaluate_graph(&edited, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        let GraphInstance::Layer { mask_weights, .. } = &inst else {
            panic!("layer");
        };
        assert_eq!(mask_weights[0], edited_expected);

        // The same joint count with another joint order resolves again too.
        let swapped = Skeleton::from_local_bind(
            vec!["arm".into(), "root".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("valid");
        let swapped_expected = edited_mask
            .resolve(&swapped.joint_names)
            .weights(&swapped.parents, swapped.len());
        let mut ctx = EvalContext::new(
            &swapped,
            &lib,
            &params,
            0.1,
            Entity::PLACEHOLDER,
            &sockets,
            Vec::new(),
        );
        evaluate_graph(&edited, &mut inst, &mut ctx, &mut out).expect("eval");
        ctx.finish();
        let GraphInstance::Layer { mask_weights, .. } = &inst else {
            panic!("layer");
        };
        assert_eq!(mask_weights[0], swapped_expected);
    }

    #[test]
    fn curve_and_node_morph_values_survive_reused_scratch() {
        let sk = skeleton2();
        let mut clip = slide_clip("Face", 0.0, 1.0);
        clip.curves.push(crate::clip::NamedCurve {
            name: "Blink".into(),
            curve: TrackCurve::Static(0.4),
        });
        clip.morph_tracks
            .push(crate::clip::MorphTrack::new(3, TrackCurve::Static(0.2)));
        clip.morph_tracks.push(crate::clip::MorphTrack::for_node(
            crate::clip::MorphNode {
                index: 2,
                name: "head".into(),
            },
            1,
            TrackCurve::Static(0.7),
        ));
        let lib = library(vec![clip]);
        let graph = AnimGraph {
            parameters: vec![],
            root: clip_node("Face", true, 1.0),
        };
        let params = ParameterSet::new();
        let sockets = HashMap::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut kept = None;
        let mut name_cap = None;
        let mut raw_cap = None;
        for step in 0..4 {
            let mut ctx =
                EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
            if let Some((curve_sample, curves, node_raw, shared, nodes, ik)) = kept.take() {
                ctx = ctx.with_eval_scratch(curve_sample, curves, node_raw, shared, nodes, ik);
            }
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            let finished = ctx.finish();
            assert_eq!(finished.curves.len(), 1);
            assert_eq!(finished.curves[0].0, "Blink");
            assert!((finished.curves[0].1 - 0.4).abs() < 1e-5);
            assert_eq!(finished.morphs, vec![(3, 0.2)]);
            assert_eq!(finished.node_morphs, vec![(2, 1, 0.7)]);
            assert!(!finished.curve_sample.is_empty());
            if step == 1 {
                let names = finished.curve_sample[0].0.capacity();
                let raw = finished.node_raw.capacity();
                assert!(names >= "Blink".len());
                assert!(raw >= 1);
                name_cap = Some(names);
                raw_cap = Some(raw);
            }
            if step > 1 {
                assert_eq!(finished.curve_sample[0].0.capacity(), name_cap.expect("cap"));
                assert_eq!(finished.node_raw.capacity(), raw_cap.expect("raw"));
            }
            kept = Some((
                finished.curve_sample,
                finished.curves,
                finished.node_raw,
                finished.morph_shared_pool,
                finished.morph_nodes_pool,
                finished.ik_globals,
            ));
        }
    }

    /// Audit (corr-animation): a curve slot kept from the previous frame (weight 0) was updated
    /// as `c + (v - c) * 1`, which is not `v` in f32: 1.0 then 1e-8 read 0.0.
    #[test]
    fn a_reused_curve_slot_takes_this_frames_value_exactly() {
        let sk = skeleton2();
        let lib = library(vec![]);
        let params = ParameterSet::new();
        let sockets = HashMap::new();
        let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, Vec::new());
        ctx.curves.push(("C".into(), 1.0));
        ctx.record_curve("C", 1.0e-8, 1.0);
        assert_eq!(ctx.curves[0].1, 1.0e-8);
        // A second sample this frame still blends by weight.
        ctx.record_curve("C", 1.0, 1.0);
        assert!((ctx.curves[0].1 - 0.5).abs() < 1e-6, "{}", ctx.curves[0].1);
    }

    /// Audit (corr-animation): every blend of two morph-less inputs pushed an extra empty list
    /// into the animator's pool, so the pool grew by one entry per blend per frame.
    #[test]
    fn morph_pools_stay_bounded_over_many_frames_of_morph_less_blends() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("Walk", 1.0, 1.0), slide_clip("Run", 3.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], false),
        };
        let mut params = ParameterSet::new();
        params.set_float("Speed", 0.5);
        let sockets = HashMap::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut kept = None;
        let mut pooled = (0, 0);
        for _ in 0..200 {
            let mut ctx = EvalContext::new(
                &sk,
                &lib,
                &params,
                0.016,
                Entity::PLACEHOLDER,
                &sockets,
                Vec::new(),
            );
            if let Some((curve_sample, curves, node_raw, shared, nodes, ik)) = kept.take() {
                ctx = ctx.with_eval_scratch(curve_sample, curves, node_raw, shared, nodes, ik);
            }
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            let finished = ctx.finish();
            pooled = (finished.morph_shared_pool.len(), finished.morph_nodes_pool.len());
            kept = Some((
                finished.curve_sample,
                finished.curves,
                finished.node_raw,
                finished.morph_shared_pool,
                finished.morph_nodes_pool,
                finished.ik_globals,
            ));
        }
        assert!(pooled.0 <= 8 && pooled.1 <= 8, "{pooled:?}");
    }

    #[test]
    fn blendspace_eval_reuses_weight_scratch() {
        let sk = skeleton2();
        let lib = library(vec![slide_clip("Walk", 1.0, 1.0), slide_clip("Run", 3.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: space_1d("Speed", &[(0.0, "Walk"), (1.0, "Run")], false),
        };
        let mut params = ParameterSet::new();
        params.set_float("Speed", 0.5);
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let mut caps = None;
        for step in 0..8 {
            frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
            if step == 1 {
                let GraphInstance::BlendSpace1D(runtime) = &inst else {
                    panic!("blend space");
                };
                assert!(runtime.order.capacity() >= 2);
                assert!(runtime.weights.capacity() >= 1);
                assert!(runtime.clips.capacity() >= 2);
                caps = Some((
                    runtime.order.capacity(),
                    runtime.weights.capacity(),
                    runtime.clips.capacity(),
                ));
            }
        }
        let GraphInstance::BlendSpace1D(runtime) = &inst else {
            panic!("blend space");
        };
        assert_eq!(
            (
                runtime.order.capacity(),
                runtime.weights.capacity(),
                runtime.clips.capacity()
            ),
            caps.expect("step 2")
        );
        // Eight steps of 0.1 s land at t = 0.8. Walk is 1 m over 1 s, run is 3 m; weight 0.5.
        assert!(
            (out.translations[0].x - 1.6).abs() < 1e-4,
            "{}",
            out.translations[0].x
        );
    }

    #[test]
    fn evaluate_graph_enters_a_span() {
        use std::collections::BTreeSet;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Metadata, Subscriber};

        #[derive(Clone, Default)]
        struct Recorder {
            names: Arc<Mutex<BTreeSet<&'static str>>>,
            next: Arc<AtomicU64>,
        }

        impl Subscriber for Recorder {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, attrs: &Attributes<'_>) -> Id {
                if let Ok(mut names) = self.names.lock() {
                    names.insert(attrs.metadata().name());
                }
                Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, _: &Event<'_>) {}
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        let sk = skeleton2();
        let (lib, params, scratch, sockets) = setup(&sk, vec![slide_clip("Slide", 1.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: clip_node("Slide", true, 1.0),
        };
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        let recorder = Recorder::default();
        let names = recorder.names.clone();
        tracing::subscriber::with_default(recorder, || {
            // tracing caches one interest per callsite for the process: a parallel test that ran
            // `evaluate_graph` first with no subscriber leaves it at "never", and this thread's
            // subscriber would see no span (seen on the M5, ADR-5550). Rebuild it under ours.
            // Register the callsite first (one evaluation): a callsite another test thread
            // registers for the first time after our rebuild takes that thread's "never" while
            // only one scoped subscriber is alive (flaky on the test machine, ADR-8735). Only the
            // second evaluation's spans count.
            let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            let scratch = ctx.finish().scratch;
            if let Ok(mut names) = names.lock() {
                names.clear();
            }
            tracing::callsite::rebuild_interest_cache();
            let mut ctx = EvalContext::new(&sk, &lib, &params, 0.1, Entity::PLACEHOLDER, &sockets, scratch);
            evaluate_graph(&graph, &mut inst, &mut ctx, &mut out).expect("eval");
            ctx.finish();
        });
        let seen = names.lock().map(|n| n.clone()).unwrap_or_default();
        assert!(seen.contains("evaluate_graph"), "spans: {seen:?}");
    }

    /// Spec 17 § 3 (ADR-871): an explicit `from: X, to: X` transition restarts state X, unlike
    /// an any-state transition into the current state.
    #[test]
    fn an_explicit_self_transition_restarts_the_state() {
        let sk = skeleton2();
        // 2 m over 1 s, not looping: x is the clip time × 2.
        let lib = library(vec![slide_clip("Attack", 2.0, 1.0)]);
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Attack".into(),
                states: vec![("Attack".into(), clip_node("Attack", false, 1.0))],
                transitions: vec![transition(
                    Some("Attack"),
                    "Attack",
                    vec![Condition::Trigger {
                        param: "Again".into(),
                    }],
                    0.0,
                )],
            }),
        };
        let mut params = ParameterSet::new();
        let mut inst = GraphInstance::from_node(&graph.root);
        let mut out = Pose::from_identity(sk.len());
        for _ in 0..4 {
            frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        }
        assert!(
            (out.translations[0].x - 0.8).abs() < 1e-4,
            "{}",
            out.translations[0].x
        );
        params.set_trigger("Again");
        let fired = frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        params.consume_trigger("Again");
        assert!(
            fired
                .events
                .iter()
                .any(|e| e.from.as_deref() == Some("Attack") && e.to == "Attack"),
            "self-transition event: {:?}",
            fired
                .events
                .iter()
                .map(|e| (e.from.clone(), e.to.clone()))
                .collect::<Vec<_>>()
        );
        // The fire frame still shows the source pose (0.5 s in), like any transition.
        assert!(
            (out.translations[0].x - 1.0).abs() < 1e-4,
            "fire frame: x {}",
            out.translations[0].x
        );
        // Next frame: the restarted clip is 0.2 s in (x 0.4), not 0.6 s (x 1.2).
        frame(&graph, &mut inst, &sk, &lib, &params, 0.1, &mut out);
        assert!(
            (out.translations[0].x - 0.4).abs() < 1e-4,
            "after the restart: x {}",
            out.translations[0].x
        );
    }
}
