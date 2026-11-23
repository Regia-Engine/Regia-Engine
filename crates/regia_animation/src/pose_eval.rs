// SPDX-License-Identifier: MPL-2.0
//! Side-effect-free pose evaluation (ADR-2551): the pose an [`Animator`] would produce for a
//! recorded playback state, parameter set and time step, without touching the animator.
//!
//! Lag compensation (spec 75 § 4) records what an animator *was* on each server tick and poses
//! joint-attached hit shapes from that record only when a rewound query needs them; a remote
//! character (spec 74) is driven from the playback state its server replicated. Both need the
//! same three pieces:
//!
//! | Piece | What |
//! |---|---|
//! | [`AnimStateSnapshot`] | a compact copy of the animator's playback instance (clip clocks, states, transitions, layer weights) |
//! | [`ParamLayout`] / [`ParamLanes`] | the parameter set as a shared name layout plus one 32-bit lane per value |
//! | [`evaluate_pose`] | evaluates the graph from a snapshot into a [`Pose`], firing no event and writing nothing |
//!
//! [`capture_state`] reads an animator; [`restore_state`] writes a snapshot back into one (a
//! remote character that re-anchors on its server's clock).
//!
//! # Exactness
//!
//! The animator evaluates `instance(before) + parameters + delta -> pose`. A snapshot taken
//! before an evaluation, evaluated with the same parameters and the same delta, gives the same
//! pose bit for bit: [`evaluate_pose`] runs the same graph code on an instance rebuilt from the
//! snapshot. What the snapshot does not hold is refused at capture ([`CaptureError`]): an
//! inertialized transition (its offset is a whole pose), declared sync groups (cross-node
//! state) and an animator that has not been evaluated yet. IK nodes read socket positions from
//! the world: [`PoseRequest::socket_targets`] passes them, `None` leaves IK targets unresolved
//! (the pose before IK). Montages blend after the graph and are not part of this evaluation.

pub mod demo;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use regia_core::math::{Quat, Vec2, Vec3};
use regia_ecs::prelude::Entity;

use crate::components::{Animator, AnimatorSource};
use crate::eval::{
    ActiveTransition, BlendRuntime, ClipState, EvalContext, EvalError, GraphInstance, StateMachineInstance,
};
use crate::graph::{AnimGraph, AnimNode};
use crate::inertialize::WeightInertia;
use crate::params::{ParamValue, ParameterSet};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::state_machine::{StateMachineDef, TransitionBlend};

/// Version of the [`AnimStateSnapshot::words`] layout (first word of every snapshot).
pub const STATE_WORDS_VERSION: u32 = 1;

/// Source kind word: a graph.
const SOURCE_GRAPH: u32 = 0;
/// Source kind word: a single clip.
const SOURCE_SINGLE: u32 = 1;

/// Most words a snapshot may hold (a malformed word list must not allocate without bound).
pub const MAX_STATE_WORDS: usize = 4096;

// ---- parameters ---------------------------------------------------------------------------------

/// The type of one parameter in a [`ParamLayout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParamKind {
    /// `f32`, one lane (its bits).
    Float,
    /// `i32`, one lane (its bits).
    Int,
    /// `bool`, one lane (0 or 1).
    Bool,
    /// Two `f32` lanes.
    Vec2,
    /// Three `f32` lanes.
    Vec3,
    /// A trigger, one lane (1 = set, waiting to be consumed).
    Trigger,
}

impl ParamKind {
    /// Lanes a value of this kind takes.
    pub fn lanes(self) -> usize {
        match self {
            Self::Vec2 => 2,
            Self::Vec3 => 3,
            _ => 1,
        }
    }

    /// Whether its lanes are `f32` bits that blend linearly (floats and vectors).
    pub fn blends(self) -> bool {
        matches!(self, Self::Float | Self::Vec2 | Self::Vec3)
    }

    fn of(value: &ParamValue) -> Self {
        match value {
            ParamValue::Float(_) => Self::Float,
            ParamValue::Int(_) => Self::Int,
            ParamValue::Bool(_) => Self::Bool,
            ParamValue::Vec2(_) => Self::Vec2,
            ParamValue::Vec3(_) => Self::Vec3,
            ParamValue::Trigger => Self::Trigger,
        }
    }
}

/// Layouts [`ParamLayout::declared_shared`] keeps.
const DECLARED_CACHE_CAP: usize = 64;

struct DeclaredEntry {
    fingerprint: u64,
    layout: Arc<ParamLayout>,
}

fn declared_cache() -> &'static Mutex<Vec<DeclaredEntry>> {
    static CACHE: Mutex<Vec<DeclaredEntry>> = Mutex::new(Vec::new());
    &CACHE
}

/// FNV-1a of declaration names and kinds. A hit still compares the names, so a collision
/// cannot return the wrong layout.
fn declared_fingerprint(graph: &AnimGraph) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for decl in &graph.parameters {
        for byte in decl.0.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= u64::from(kind_tag(ParamKind::of(&decl.1)));
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn kind_tag(kind: ParamKind) -> u8 {
    match kind {
        ParamKind::Float => 1,
        ParamKind::Int => 2,
        ParamKind::Bool => 3,
        ParamKind::Vec2 => 4,
        ParamKind::Vec3 => 5,
        ParamKind::Trigger => 6,
    }
}

fn declared_matches(layout: &ParamLayout, graph: &AnimGraph) -> bool {
    layout.entries.len() == graph.parameters.len()
        && layout
            .entries
            .iter()
            .zip(&graph.parameters)
            .all(|((name, kind), decl)| name == &decl.0 && *kind == ParamKind::of(&decl.1))
}

/// The names and kinds of a parameter set, in lane order. Shared (`Arc`) between the ticks of a
/// history and the two ends of a link, so the lanes themselves carry no names.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ParamLayout {
    entries: Vec<(String, ParamKind)>,
}

impl ParamLayout {
    /// Every value stored in `set`, sorted by name then kind (triggers included): lanes of this
    /// layout rebuild the set exactly.
    pub fn of_set(set: &ParameterSet) -> Self {
        let mut entries: Vec<(String, ParamKind)> = set
            .values()
            .into_iter()
            .map(|(name, value)| (name, ParamKind::of(&value)))
            .chain(
                set.triggers()
                    .into_iter()
                    .map(|(name, _)| (name, ParamKind::Trigger)),
            )
            .collect();
        entries.sort();
        Self { entries }
    }

    /// The graph's declared parameters, in declaration order (what both ends of a link know
    /// without sending names). A parameter the set lacks reads as its type's zero, which is what
    /// the graph reads for an absent value too.
    pub fn declared(graph: &AnimGraph) -> Self {
        Self {
            entries: graph
                .parameters
                .iter()
                .map(|d| (d.0.clone(), ParamKind::of(&d.1)))
                .collect(),
        }
    }

    /// The graph's declared parameters, shared across callers that pass the same declarations.
    ///
    /// [`declared`](Self::declared) builds a fresh layout. Frame paths (a net capture, a net
    /// drive) call this so the declaration names are allocated once per distinct graph. The
    /// cache is process-wide, keyed by a fingerprint of the names and kinds, and checked
    /// against the graph on a hit. It keeps the 64 most recently used layouts.
    pub fn declared_shared(graph: &AnimGraph) -> Arc<ParamLayout> {
        let fingerprint = declared_fingerprint(graph);
        let mut guard = declared_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pos) = guard.iter().position(|entry| entry.fingerprint == fingerprint) {
            if declared_matches(&guard[pos].layout, graph) {
                let layout = Arc::clone(&guard[pos].layout);
                let entry = guard.remove(pos);
                guard.push(entry);
                return layout;
            }
            guard.remove(pos);
        }
        let layout = Arc::new(Self::declared(graph));
        if guard.len() == DECLARED_CACHE_CAP {
            guard.remove(0);
        }
        guard.push(DeclaredEntry {
            fingerprint,
            layout: Arc::clone(&layout),
        });
        layout
    }

    /// The entries, in lane order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, ParamKind)> {
        self.entries.iter().map(|(n, k)| (n.as_str(), *k))
    }

    /// Number of parameters.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the layout is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lanes the layout's values take.
    pub fn lanes(&self) -> usize {
        self.entries.iter().map(|(_, k)| k.lanes()).sum()
    }

    /// For each lane, whether it blends linearly (`f32` lanes of floats and vectors); the others
    /// step.
    pub fn blend_mask(&self) -> Vec<bool> {
        self.entries
            .iter()
            .flat_map(|(_, k)| std::iter::repeat_n(k.blends(), k.lanes()))
            .collect()
    }

    /// `set`'s values in this layout's lanes (a value the set lacks reads as zero).
    pub fn encode(&self, set: &ParameterSet) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.lanes());
        self.encode_into(set, &mut out);
        out
    }

    /// [`encode`](Self::encode) written into `out` (cleared first). A buffer kept between ticks
    /// makes no heap request once its capacity covers the lanes.
    pub fn encode_into(&self, set: &ParameterSet, out: &mut Vec<u32>) {
        out.clear();
        for (name, kind) in &self.entries {
            match kind {
                ParamKind::Float => out.push(set.float(name).to_bits()),
                ParamKind::Int => out.push(set.int(name) as u32),
                ParamKind::Bool => out.push(u32::from(set.bool(name))),
                ParamKind::Trigger => out.push(u32::from(set.trigger(name))),
                ParamKind::Vec2 => {
                    let v = set.vec2(name);
                    out.extend([v.x.to_bits(), v.y.to_bits()]);
                }
                ParamKind::Vec3 => {
                    let v = set.vec3(name);
                    out.extend([v.x.to_bits(), v.y.to_bits(), v.z.to_bits()]);
                }
            }
        }
    }

    /// Writes `lanes` into `set` (missing lanes leave their parameters unchanged). A trigger lane
    /// of 1 sets the trigger; 0 consumes it.
    pub fn apply(&self, lanes: &[u32], set: &mut ParameterSet) {
        let mut at = 0usize;
        let f = |i: usize| lanes.get(i).copied().map(f32::from_bits);
        for (name, kind) in &self.entries {
            let n = kind.lanes();
            if at + n > lanes.len() {
                return;
            }
            match kind {
                ParamKind::Float => set.set_float(name, f32::from_bits(lanes[at])),
                ParamKind::Int => set.set_int(name, lanes[at] as i32),
                ParamKind::Bool => set.set_bool(name, lanes[at] != 0),
                ParamKind::Trigger => {
                    if lanes[at] != 0 {
                        set.set_trigger(name);
                    } else if set.trigger(name) {
                        set.consume_trigger(name);
                    } else if set.get(name).is_none() {
                        // Present but unset: a consumed trigger stays declared.
                        set.set_trigger(name);
                        set.consume_trigger(name);
                    }
                }
                ParamKind::Vec2 => {
                    if let (Some(x), Some(y)) = (f(at), f(at + 1)) {
                        set.set_vec2(name, Vec2::new(x, y));
                    }
                }
                ParamKind::Vec3 => {
                    if let (Some(x), Some(y), Some(z)) = (f(at), f(at + 1), f(at + 2)) {
                        set.set_vec3(name, Vec3::new(x, y, z));
                    }
                }
            }
            at += n;
        }
    }

    /// Writes only the blending lanes (floats and vectors) of `lanes` into `set`: between two
    /// re-anchors a driven animator takes interpolated values without re-firing triggers or
    /// re-stepping flags.
    pub fn apply_blending(&self, lanes: &[u32], set: &mut ParameterSet) {
        let mut at = 0usize;
        for (name, kind) in &self.entries {
            let n = kind.lanes();
            let Some(v) = lanes.get(at..at + n) else {
                return;
            };
            match kind {
                ParamKind::Float => set.set_float(name, f32::from_bits(v[0])),
                ParamKind::Vec2 => set.set_vec2(name, Vec2::new(f32::from_bits(v[0]), f32::from_bits(v[1]))),
                ParamKind::Vec3 => set.set_vec3(
                    name,
                    Vec3::new(f32::from_bits(v[0]), f32::from_bits(v[1]), f32::from_bits(v[2])),
                ),
                _ => {}
            }
            at += n;
        }
    }

    /// A fresh parameter set holding exactly `lanes`.
    pub fn decode(&self, lanes: &[u32]) -> ParameterSet {
        let mut set = ParameterSet::new();
        self.apply(lanes, &mut set);
        set
    }
}

/// A parameter set as lanes of a shared layout: what a history keeps per tick.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamLanes {
    /// The names and kinds (shared between ticks while it does not change).
    pub layout: Arc<ParamLayout>,
    /// One 32-bit lane per value component.
    pub values: Box<[u32]>,
}

impl ParamLanes {
    /// Captures `set` exactly ([`ParamLayout::of_set`]). `reuse` is the previous capture's
    /// layout: when the set still has the same names and kinds it is shared instead of copied.
    pub fn capture(set: &ParameterSet, reuse: Option<&Arc<ParamLayout>>) -> Self {
        let fresh = ParamLayout::of_set(set);
        let layout = match reuse {
            Some(l) if **l == fresh => l.clone(),
            _ => Arc::new(fresh),
        };
        let values = layout.encode(set).into_boxed_slice();
        Self { layout, values }
    }

    /// The parameter set back.
    pub fn to_set(&self) -> ParameterSet {
        self.layout.decode(&self.values)
    }

    /// Heap bytes held per capture (the lanes; the layout is shared).
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.values)
    }
}

// ---- snapshots ----------------------------------------------------------------------------------

/// Why an animator's state could not be captured (protocol codes for agents and logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The animator has not been evaluated yet: `anim_not_evaluated`.
    #[error("the animator has not been evaluated yet")]
    NotEvaluated,
    /// A state machine runs an inertialized transition (its offset is a whole pose):
    /// `anim_inertialized`.
    #[error("an inertialized transition holds a pose offset a snapshot does not keep")]
    Inertialized,
    /// The animator declares cross-node sync groups: `anim_sync_groups`.
    #[error("sync groups carry cross-node state a snapshot does not keep")]
    SyncGroups,
    /// The playback instance does not fit the graph (a reload in flight): `anim_shape_mismatch`.
    #[error("the playback instance does not fit the graph")]
    ShapeMismatch,
}

impl CaptureError {
    /// The protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotEvaluated => "anim_not_evaluated",
            Self::Inertialized => "anim_inertialized",
            Self::SyncGroups => "anim_sync_groups",
            Self::ShapeMismatch => "anim_shape_mismatch",
        }
    }
}

/// A compact copy of an animator's playback instance: clip clocks and flags, state-machine
/// states and transitions, layer weight easing, as 32-bit words in node order. No pose.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AnimStateSnapshot {
    words: Box<[u32]>,
}

impl AnimStateSnapshot {
    /// The words: `[STATE_WORDS_VERSION, source kind, node words...]`.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// A snapshot from words (as replicated: the words are the whole state).
    pub fn from_words(words: &[u32]) -> Self {
        Self {
            words: words.iter().copied().take(MAX_STATE_WORDS).collect(),
        }
    }

    /// Heap bytes this snapshot holds.
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.words)
    }
}

fn push_clip(st: &ClipState, w: &mut Vec<u32>) {
    w.push(st.time.to_bits());
    w.push(u32::from(st.finished) | (u32::from(st.started) << 1));
}

fn blend_code(b: TransitionBlend) -> u32 {
    match b {
        TransitionBlend::Crossfade => 0,
        TransitionBlend::Inertialize => 1,
        TransitionBlend::InertializeSpring => 2,
    }
}

fn encode_node(node: &AnimNode, inst: &GraphInstance, w: &mut Vec<u32>) -> Result<(), CaptureError> {
    match (node, inst) {
        (AnimNode::Clip(_), GraphInstance::Clip(st)) => push_clip(st, w),
        (AnimNode::BlendSpace1D(_), GraphInstance::BlendSpace1D(sts))
        | (AnimNode::BlendSpace2D(_), GraphInstance::BlendSpace2D(sts)) => {
            if !inst.matches_node(node) {
                return Err(CaptureError::ShapeMismatch);
            }
            w.push(sts.states.len() as u32);
            for st in &sts.states {
                push_clip(st, w);
            }
        }
        (AnimNode::StateMachine(def), GraphInstance::StateMachine(sm)) => {
            if sm.tracks_history || sm.history_valid || sm.inertia.is_active() {
                return Err(CaptureError::Inertialized);
            }
            if sm.states.len() != def.states.len() {
                return Err(CaptureError::ShapeMismatch);
            }
            w.push(sm.current as u32);
            w.push(u32::from(sm.entered_initial) | (u32::from(sm.transition.is_some()) << 1));
            if let Some(t) = &sm.transition {
                if t.blend.is_inertial() {
                    return Err(CaptureError::Inertialized);
                }
                w.extend([
                    t.def as u32,
                    t.from as u32,
                    t.elapsed.to_bits(),
                    blend_code(t.blend),
                ]);
            }
            for (state, (_, node)) in sm.states.iter().zip(&def.states) {
                encode_node(node, state, w)?;
            }
        }
        (AnimNode::Blend(def), GraphInstance::Blend { a, b }) => {
            encode_node(&def.a, a, w)?;
            encode_node(&def.b, b, w)?;
        }
        (AnimNode::Additive(def), GraphInstance::Additive { base, additive }) => {
            encode_node(&def.base, base, w)?;
            encode_node(&def.additive, additive, w)?;
        }
        (
            AnimNode::Layer(def),
            GraphInstance::Layer {
                base,
                layers,
                weights: ws,
                ..
            },
        ) => {
            if layers.len() != def.layers.len() || ws.len() != def.layers.len() {
                return Err(CaptureError::ShapeMismatch);
            }
            encode_node(&def.base, base, w)?;
            for (inst, entry) in layers.iter().zip(&def.layers) {
                encode_node(&entry.graph, inst, w)?;
            }
            for weight in ws {
                w.extend_from_slice(&weight.to_words());
            }
        }
        (AnimNode::TwoBoneIK(n), GraphInstance::Wrap(inner)) => encode_node(&n.inner, inner, w)?,
        (AnimNode::LookAt(n), GraphInstance::Wrap(inner)) => encode_node(&n.inner, inner, w)?,
        (AnimNode::RootMotion(n), GraphInstance::Wrap(inner)) => encode_node(&n.inner, inner, w)?,
        (AnimNode::Slot(n), GraphInstance::Wrap(inner)) => encode_node(&n.source, inner, w)?,
        (AnimNode::ChainIK(n), GraphInstance::Wrap(inner)) => encode_node(&n.inner, inner, w)?,
        (AnimNode::BodyAim(n), GraphInstance::Wrap(inner)) => encode_node(&n.inner, inner, w)?,
        // The warp nodes' smoothed ratio is not captured: a decoded instance starts at ratio 1.
        (AnimNode::StrideWarp(n), GraphInstance::Warp { inner, .. }) => encode_node(&n.inner, inner, w)?,
        (AnimNode::OrientationWarp(n), GraphInstance::Warp { inner, .. }) => encode_node(&n.inner, inner, w)?,
        _ => return Err(CaptureError::ShapeMismatch),
    }
    Ok(())
}

/// Reads words in order; every read is bounds-checked.
struct Words<'a> {
    words: &'a [u32],
    at: usize,
}

impl Words<'_> {
    fn next(&mut self) -> Result<u32, PoseEvalError> {
        let w = self.words.get(self.at).copied().ok_or(PoseEvalError::BadState)?;
        self.at += 1;
        Ok(w)
    }

    /// The next word as a finite `f32` (words come from a link: a NaN clock is refused).
    fn finite(&mut self) -> Result<f32, PoseEvalError> {
        let v = f32::from_bits(self.next()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(PoseEvalError::BadState)
        }
    }

    fn clip(&mut self) -> Result<ClipState, PoseEvalError> {
        let time = self.finite()?;
        let flags = self.next()?;
        Ok(ClipState {
            time,
            finished: flags & 1 != 0,
            started: flags & 2 != 0,
        })
    }

    fn weight(&mut self) -> Result<WeightInertia, PoseEvalError> {
        let end = self.at + WeightInertia::WORDS;
        let w: &[u32; WeightInertia::WORDS] = self
            .words
            .get(self.at..end)
            .and_then(|s| s.try_into().ok())
            .ok_or(PoseEvalError::BadState)?;
        // Every word but the last (the `initialised` flag) is an `f32`.
        if w[..WeightInertia::WORDS - 1]
            .iter()
            .any(|b| !f32::from_bits(*b).is_finite())
        {
            return Err(PoseEvalError::BadState);
        }
        self.at = end;
        Ok(WeightInertia::from_words(w))
    }

    fn index(&mut self, len: usize) -> Result<usize, PoseEvalError> {
        let i = self.next()? as usize;
        if i < len {
            Ok(i)
        } else {
            Err(PoseEvalError::BadState)
        }
    }
}

fn decode_machine(def: &StateMachineDef, r: &mut Words<'_>) -> Result<StateMachineInstance, PoseEvalError> {
    let mut sm = StateMachineInstance::from_def(def);
    sm.current = r.index(def.states.len())?;
    let flags = r.next()?;
    sm.entered_initial = flags & 1 != 0;
    sm.transition = if flags & 2 != 0 {
        let def_index = r.index(def.transitions.len())?;
        let from = r.index(def.states.len())?;
        let elapsed = r.finite()?;
        let blend = match r.next()? {
            0 => TransitionBlend::Crossfade,
            _ => return Err(PoseEvalError::BadState),
        };
        Some(ActiveTransition {
            def: def_index,
            from,
            elapsed,
            blend,
        })
    } else {
        None
    };
    sm.states = def
        .states
        .iter()
        .map(|(_, node)| decode_node(node, r))
        .collect::<Result<_, _>>()?;
    Ok(sm)
}

fn decode_node(node: &AnimNode, r: &mut Words<'_>) -> Result<GraphInstance, PoseEvalError> {
    Ok(match node {
        AnimNode::Clip(_) => GraphInstance::Clip(r.clip()?),
        AnimNode::BlendSpace1D(n) => {
            let count = r.next()? as usize;
            if count != n.space.samples.len() + 1 {
                return Err(PoseEvalError::BadState);
            }
            GraphInstance::BlendSpace1D(BlendRuntime::from_states(
                (0..count).map(|_| r.clip()).collect::<Result<_, _>>()?,
            ))
        }
        AnimNode::BlendSpace2D(n) => {
            let count = r.next()? as usize;
            if count != n.space.samples.len() + 1 {
                return Err(PoseEvalError::BadState);
            }
            GraphInstance::BlendSpace2D(BlendRuntime::from_states(
                (0..count).map(|_| r.clip()).collect::<Result<_, _>>()?,
            ))
        }
        AnimNode::StateMachine(def) => GraphInstance::StateMachine(Box::new(decode_machine(def, r)?)),
        AnimNode::Blend(def) => GraphInstance::Blend {
            a: Box::new(decode_node(&def.a, r)?),
            b: Box::new(decode_node(&def.b, r)?),
        },
        AnimNode::Additive(def) => GraphInstance::Additive {
            base: Box::new(decode_node(&def.base, r)?),
            additive: Box::new(decode_node(&def.additive, r)?),
        },
        AnimNode::Layer(def) => {
            let base = Box::new(decode_node(&def.base, r)?);
            let layers = def
                .layers
                .iter()
                .map(|l| decode_node(&l.graph, r))
                .collect::<Result<Vec<_>, _>>()?;
            let weights = (0..def.layers.len())
                .map(|_| r.weight())
                .collect::<Result<Vec<_>, _>>()?;
            GraphInstance::Layer {
                base,
                layers,
                weights,
                mask_weights: Vec::new(),
                mask_joints: usize::MAX,
            }
        }
        AnimNode::TwoBoneIK(n) => GraphInstance::Wrap(Box::new(decode_node(&n.inner, r)?)),
        AnimNode::LookAt(n) => GraphInstance::Wrap(Box::new(decode_node(&n.inner, r)?)),
        AnimNode::RootMotion(n) => GraphInstance::Wrap(Box::new(decode_node(&n.inner, r)?)),
        AnimNode::Slot(n) => GraphInstance::Wrap(Box::new(decode_node(&n.source, r)?)),
        AnimNode::ChainIK(n) => GraphInstance::Wrap(Box::new(decode_node(&n.inner, r)?)),
        AnimNode::BodyAim(n) => GraphInstance::Wrap(Box::new(decode_node(&n.inner, r)?)),
        AnimNode::StrideWarp(n) => GraphInstance::Warp {
            inner: Box::new(decode_node(&n.inner, r)?),
            state: Box::default(),
        },
        AnimNode::OrientationWarp(n) => GraphInstance::Warp {
            inner: Box::new(decode_node(&n.inner, r)?),
            state: Box::default(),
        },
    })
}

/// The playback instance a snapshot describes, for `source`.
fn instance_of(source: &AnimatorSource, state: &AnimStateSnapshot) -> Result<GraphInstance, PoseEvalError> {
    let mut r = Words {
        words: &state.words,
        at: 0,
    };
    if r.next()? != STATE_WORDS_VERSION {
        return Err(PoseEvalError::BadState);
    }
    let kind = r.next()?;
    let inst = match (source, kind) {
        (AnimatorSource::Graph { graph, .. }, SOURCE_GRAPH) => decode_node(&graph.root, &mut r)?,
        (AnimatorSource::SingleClip { .. }, SOURCE_SINGLE) => GraphInstance::Clip(r.clip()?),
        _ => return Err(PoseEvalError::BadState),
    };
    if r.at != state.words.len() {
        return Err(PoseEvalError::BadState);
    }
    Ok(inst)
}

/// Captures the animator's playback instance as it is now: the state its next evaluation
/// starts from. Reads the animator only.
///
/// # Errors
///
/// [`CaptureError`] when the state is not one a snapshot can reproduce exactly.
pub fn capture_state(animator: &Animator) -> Result<AnimStateSnapshot, CaptureError> {
    if animator.sync_setup.is_some() {
        return Err(CaptureError::SyncGroups);
    }
    let inst = animator.instance.as_ref().ok_or(CaptureError::NotEvaluated)?;
    snapshot_of(&animator.source, inst)
}

/// [`capture_state`] written into `words` (cleared first) instead of a new snapshot: the same
/// words [`AnimStateSnapshot::words`] would hold. A caller that keeps `words` between ticks makes
/// no heap request once its capacity covers the state.
///
/// # Errors
///
/// [`CaptureError`] as [`capture_state`]; `words` then holds no usable state.
pub fn capture_state_into(animator: &Animator, words: &mut Vec<u32>) -> Result<(), CaptureError> {
    words.clear();
    if animator.sync_setup.is_some() {
        return Err(CaptureError::SyncGroups);
    }
    let inst = animator.instance.as_ref().ok_or(CaptureError::NotEvaluated)?;
    write_snapshot(&animator.source, inst, words)
}

fn snapshot_of(source: &AnimatorSource, inst: &GraphInstance) -> Result<AnimStateSnapshot, CaptureError> {
    let mut words = Vec::new();
    write_snapshot(source, inst, &mut words)?;
    Ok(AnimStateSnapshot {
        words: words.into_boxed_slice(),
    })
}

fn write_snapshot(
    source: &AnimatorSource,
    inst: &GraphInstance,
    words: &mut Vec<u32>,
) -> Result<(), CaptureError> {
    words.clear();
    words.push(STATE_WORDS_VERSION);
    match source {
        AnimatorSource::Graph { graph, .. } => {
            words.push(SOURCE_GRAPH);
            encode_node(&graph.root, inst, words)?;
        }
        AnimatorSource::SingleClip { .. } => {
            words.push(SOURCE_SINGLE);
            let GraphInstance::Clip(st) = inst else {
                return Err(CaptureError::ShapeMismatch);
            };
            push_clip(st, words);
        }
    }
    if words.len() > MAX_STATE_WORDS {
        return Err(CaptureError::ShapeMismatch);
    }
    Ok(())
}

/// The delta the animator's next evaluation uses when `seconds` of game time pass (its speed
/// applied, LOD time already banked included), as the evaluate system computes it.
pub fn next_eval_delta(animator: &Animator, seconds: f32) -> f32 {
    crate::num::finite_or((animator.rate_accumulator + seconds) * animator.speed, 0.0)
}

// ---- evaluation ---------------------------------------------------------------------------------

/// Why [`evaluate_pose`] or [`restore_state`] failed.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum PoseEvalError {
    /// The snapshot does not describe this source (another graph, a malformed word list):
    /// `anim_bad_state`.
    #[error("the snapshot does not fit the animator's source")]
    BadState,
    /// Graph evaluation failed (a missing clip or joint): `anim_eval_failed`.
    #[error("graph evaluation failed: {0}")]
    Eval(EvalError),
}

impl PoseEvalError {
    /// The protocol code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadState => "anim_bad_state",
            Self::Eval(_) => "anim_eval_failed",
        }
    }
}

/// Inputs of one pure evaluation.
#[derive(Clone, Copy)]
pub struct PoseRequest<'a> {
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// The graph or clip played (with its clips).
    pub source: &'a AnimatorSource,
    /// The playback state the evaluation starts from.
    pub state: &'a AnimStateSnapshot,
    /// The parameters.
    pub params: &'a ParameterSet,
    /// Seconds of animation time (speed applied: [`next_eval_delta`]).
    pub delta: f32,
    /// The rig entity (socket lookups key on it).
    pub rig: Entity,
    /// Socket positions for IK targets, in the rig's object space (as
    /// [`crate::SocketTargets`] holds them); `None` leaves IK targets unresolved.
    pub socket_targets: Option<&'a HashMap<(Entity, String), Vec3>>,
}

/// What a pure evaluation produced besides the pose.
#[derive(Clone, Debug, PartialEq)]
pub struct PoseEvalOutput {
    /// Root motion of the evaluation, rig object space.
    pub root_motion: (Vec3, Quat),
    /// Trigger parameters a transition fired on (the live animator would consume them).
    pub consumed_triggers: Vec<String>,
}

fn empty_sockets() -> &'static HashMap<(Entity, String), Vec3> {
    static EMPTY: std::sync::OnceLock<HashMap<(Entity, String), Vec3>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

/// Evaluates the pose `request` describes into `out` (resized to the skeleton, otherwise
/// overwritten by the graph as the animator's own buffer is). Fires no notify or state event,
/// consumes no trigger, accumulates no root motion and touches no animator: everything it reads
/// is borrowed immutably and everything it writes is `out`.
///
/// # Errors
///
/// [`PoseEvalError`] for a snapshot that does not fit the source, or a graph error.
pub fn evaluate_pose(request: &PoseRequest<'_>, out: &mut Pose) -> Result<PoseEvalOutput, PoseEvalError> {
    let mut instance = instance_of(request.source, request.state)?;
    if out.len() != request.skeleton.len() {
        *out = Pose::from_identity(request.skeleton.len());
    }
    run(request, &mut instance, out)
}

/// One evaluation of `instance` with the request's inputs, on a fresh context.
fn run(
    request: &PoseRequest<'_>,
    instance: &mut GraphInstance,
    out: &mut Pose,
) -> Result<PoseEvalOutput, PoseEvalError> {
    static EMPTY_CLIPS: std::sync::OnceLock<crate::eval::ClipLibrary> = std::sync::OnceLock::new();
    let clips = match request.source {
        AnimatorSource::Graph { clips, .. } => &**clips,
        AnimatorSource::SingleClip { .. } => EMPTY_CLIPS.get_or_init(crate::eval::ClipLibrary::new),
    };
    let mut ctx = EvalContext::new(
        request.skeleton,
        clips,
        request.params,
        request.delta,
        request.rig,
        request.socket_targets.unwrap_or(empty_sockets()),
        Vec::new(),
    );
    let result = match request.source {
        AnimatorSource::Graph { graph, .. } => crate::eval::evaluate_graph(graph, instance, &mut ctx, out),
        AnimatorSource::SingleClip { clip, looping } => {
            crate::eval::evaluate_single_clip(clip, *looping, instance, &mut ctx, out)
        }
    };
    result.map_err(PoseEvalError::Eval)?;
    Ok(PoseEvalOutput {
        root_motion: ctx.root_motion,
        consumed_triggers: std::mem::take(&mut ctx.consumed_triggers),
    })
}

/// Writes `state` into the animator's playback instance: its next evaluation continues from the
/// snapshot's clocks, states and layer weights (a remote character re-anchoring on its server's
/// timeline).
///
/// # Errors
///
/// [`PoseEvalError::BadState`] when the snapshot does not fit the animator's source; the
/// animator is unchanged then.
pub fn restore_state(animator: &mut Animator, state: &AnimStateSnapshot) -> Result<(), PoseEvalError> {
    let instance = instance_of(&animator.source, state)?;
    animator.instance = Some(instance);
    Ok(())
}

/// [`restore_state`], then banks `lead_seconds` of game time for the animator's next evaluation:
/// it evaluates once over the frame's delta plus the lead, as if the snapshot had been taken that
/// much earlier. A remote character shown a fraction of a tick past its newest state catches up in
/// the same single evaluation the server ran (an extra evaluation could fire a transition a tick
/// early). Non-finite or negative leads bank nothing.
///
/// # Errors
///
/// As [`restore_state`]; the animator is unchanged then.
pub fn restore_state_ahead(
    animator: &mut Animator,
    state: &AnimStateSnapshot,
    lead_seconds: f32,
) -> Result<(), PoseEvalError> {
    restore_state(animator, state)?;
    animator.rate_accumulator = crate::num::finite_or(lead_seconds, 0.0).max(0.0);
    Ok(())
}

/// Per joint, an upper bound of its distance from the rig origin while animation only rotates
/// joints: the lengths of the bind translations along its chain, summed (parents precede their
/// children, so one pass). Translation tracks can reach further; callers add slack.
pub fn bind_chain_reach(skeleton: &Skeleton) -> Vec<f32> {
    let mut reach: Vec<f32> = Vec::with_capacity(skeleton.len());
    for i in 0..skeleton.len() {
        let own = skeleton.bind_pose.get(i).map_or(0.0, |b| {
            let s = b.scale.abs().max_element();
            b.translation.length() * s.max(1.0)
        });
        let parent = skeleton
            .parents
            .get(i)
            .copied()
            .flatten()
            .and_then(|p| reach.get(p as usize).copied())
            .unwrap_or(0.0);
        reach.push(parent + own);
    }
    reach
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ParamDecl;

    #[test]
    fn param_lanes_rebuild_the_set_exactly() {
        let mut p = ParameterSet::new();
        p.set_float("Speed", 3.25);
        p.set_int("Combo", -2);
        p.set_bool("Crouch", true);
        p.set_vec2("Move", Vec2::new(0.5, -1.0));
        p.set_vec3("Aim", Vec3::new(1.0, 2.0, 3.0));
        p.set_trigger("Jump");
        p.set_trigger("Fire");
        p.consume_trigger("Fire");
        let lanes = ParamLanes::capture(&p, None);
        assert_eq!(lanes.values.len(), 1 + 1 + 1 + 2 + 3 + 1 + 1);
        assert_eq!(lanes.to_set(), p);
        let again = ParamLanes::capture(&p, Some(&lanes.layout));
        assert!(
            Arc::ptr_eq(&again.layout, &lanes.layout),
            "an unchanged layout is shared"
        );
        p.set_float("New", 1.0);
        let changed = ParamLanes::capture(&p, Some(&lanes.layout));
        assert!(!Arc::ptr_eq(&changed.layout, &lanes.layout));
    }

    #[test]
    fn declared_layout_and_blend_mask() {
        let graph = AnimGraph {
            parameters: vec![
                ParamDecl("Speed".into(), ParamValue::Float(0.0)),
                ParamDecl("Grounded".into(), ParamValue::Bool(true)),
                ParamDecl("Move".into(), ParamValue::Vec2(Vec2::ZERO)),
            ],
            root: AnimNode::Clip(crate::graph::ClipNode {
                clip: "c".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let layout = ParamLayout::declared(&graph);
        assert_eq!(layout.lanes(), 4);
        assert_eq!(layout.blend_mask(), vec![true, false, true, true]);
        let mut set = ParameterSet::new();
        set.set_float("Speed", 2.0);
        let lanes = layout.encode(&set);
        assert_eq!(lanes[0], 2.0f32.to_bits());
        assert_eq!(lanes[1], 0, "an absent bool reads false");
    }

    /// The buffer forms write exactly what the allocating forms return, over stale contents,
    /// and a refused capture leaves no words behind.
    #[test]
    fn capture_into_and_encode_into_match_the_allocating_forms() {
        let mut animator = Animator::single(crate::clip::AnimationClip::empty("c", 1.0), true);
        let mut words = vec![7, 7, 7, 7, 7, 7, 7, 7, 7];
        assert_eq!(
            capture_state_into(&animator, &mut words),
            Err(CaptureError::NotEvaluated)
        );
        assert!(words.is_empty(), "a refused capture leaves no words");
        animator.ensure_instance(1);
        words.extend([9; 12]);
        capture_state_into(&animator, &mut words).expect("evaluated");
        assert_eq!(words, capture_state(&animator).expect("evaluated").words());

        let layout = ParamLayout::of_set(&{
            let mut set = ParameterSet::new();
            set.set_float("Speed", 0.5);
            set.set_vec3("Aim", Vec3::new(1.0, 2.0, 3.0));
            set.set_bool("Armed", true);
            set
        });
        let mut set = ParameterSet::new();
        set.set_float("Speed", 2.5);
        set.set_vec3("Aim", Vec3::new(-1.0, 0.0, 4.0));
        let mut lanes = vec![u32::MAX; 9];
        layout.encode_into(&set, &mut lanes);
        assert_eq!(lanes, layout.encode(&set));
        assert_eq!(lanes.len(), layout.lanes());
    }

    #[test]
    fn malformed_words_are_refused() {
        let source = AnimatorSource::SingleClip {
            clip: Arc::new(crate::clip::AnimationClip::empty("c", 1.0)),
            looping: true,
        };
        for words in [
            &[][..],
            &[STATE_WORDS_VERSION][..],
            &[9, 1, 0, 0][..],
            &[1, 1, 0][..],
            &[1, 1, 0, 0, 7][..],
            &[1, 1, f32::NAN.to_bits(), 0][..],
        ] {
            let state = AnimStateSnapshot::from_words(words);
            assert_eq!(
                instance_of(&source, &state).err(),
                Some(PoseEvalError::BadState),
                "{words:?}"
            );
        }
        let ok = AnimStateSnapshot::from_words(&[STATE_WORDS_VERSION, SOURCE_SINGLE, 0.5f32.to_bits(), 2]);
        assert!(instance_of(&source, &ok).is_ok());
    }

    #[test]
    fn reach_sums_bind_translations_along_the_chain() {
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "spine".into(), "head".into()],
            vec![None, Some(0), Some(1)],
            vec![
                crate::pose::translated(Vec3::new(0.0, 1.0, 0.0)),
                crate::pose::translated(Vec3::new(0.0, 0.5, 0.0)),
                crate::pose::translated(Vec3::new(0.0, 0.25, 0.0)),
            ],
        )
        .expect("valid skeleton");
        assert_eq!(bind_chain_reach(&skeleton), vec![1.0, 1.5, 1.75]);
    }
}
