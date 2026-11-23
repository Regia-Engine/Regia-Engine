// SPDX-License-Identifier: MPL-2.0
//! Runtime components: `Animator`, skeleton bindings, sockets, skin palettes, morph weights
//! and root-motion output.
//!
//! Specification: `docs/specs/17-animation.md` §§ 2, 4. [`SkinnedMesh`] binds a mesh entity to
//! a [`Skeleton`] and its joint entities (Tincal engine item C1, ADR-242). It lives here, next
//! to [`Skeleton`]; the renderer reads the render-owned joint palette that
//! `regia_framework::skinning::SkinnedMeshPlugin` fills from it, so `regia_render` still
//! does not depend on animation. [`SkeletonBinding`] is the deprecated old name of the same
//! type. Components holding asset/pose data are not reflected yet — they are never serialised —
//! and gain `Reflect` when `AssetRef` exists.

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

use crate::clip::AnimationClip;
use crate::eval::{ClipLibrary, EvalReuse, GraphInstance};
use crate::graph::AnimGraph;
use crate::params::{ParamValue, ParameterSet};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// How often an animator evaluates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum UpdateMode {
    /// Every frame.
    #[default]
    Always,
    /// Only when the rig entity also has [`AnimVisible`] (set by the visibility system).
    WhenVisible,
    /// Level-of-detail: `near`, `mid` and far update rates (fractions of full rate).
    Lod {
        /// Distance up to which animators update every frame.
        near: f32,
        /// Distance up to which animators update at half rate.
        mid: f32,
        /// Update rate beyond `mid` (for example 0.25). A negative rate counts as 0 (far
        /// animators hold their pose).
        far_rate: f32,
    },
}

/// Marker that gates [`UpdateMode::WhenVisible`] animators; add it from the visibility /
/// culling system.
#[derive(Component, Debug, Default)]
pub struct AnimVisible;

/// Optional LOD camera position resource used by [`UpdateMode::Lod`] to measure distance.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct AnimLodCamera {
    /// World-space camera position.
    pub position: Vec3,
}

/// Where an animator's extracted root motion goes (`docs/specs/34-montage-extensions.md` M6).
///
/// The route changes the motion output only, never the pose. A clip whose import mode is
/// `RootMotionMode::None` contributes nothing in every route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootMotionRoute {
    /// Extracted motion is discarded: the rig is not moved by animation and
    /// [`Animator::last_root_motion`] stays zero.
    Ignore,
    /// Motion is computed and exposed through [`Animator::last_root_motion`] but not applied
    /// (the rig's `RootMotionOutput` receives nothing).
    Report,
    /// Graph clips and montages both contribute, weighted (the behavior before routes existed).
    #[default]
    GraphAndMontages,
    /// Graph clips contribute zero root motion; montage root motion applies.
    OnlyMontages,
}

impl RootMotionRoute {
    /// The graph's root motion as this route lets it into the montage stage.
    #[must_use]
    pub fn graph_input(self, graph: (Vec3, Quat)) -> (Vec3, Quat) {
        match self {
            RootMotionRoute::OnlyMontages => (Vec3::ZERO, Quat::IDENTITY),
            _ => graph,
        }
    }

    /// Splits the final root motion into `(reported, applied)`: what
    /// [`Animator::last_root_motion`] shows and what the rig's `RootMotionOutput` accumulates.
    #[must_use]
    pub fn split(self, root: (Vec3, Quat)) -> ((Vec3, Quat), (Vec3, Quat)) {
        let none = (Vec3::ZERO, Quat::IDENTITY);
        match self {
            RootMotionRoute::Ignore => (none, none),
            RootMotionRoute::Report => (root, none),
            RootMotionRoute::GraphAndMontages | RootMotionRoute::OnlyMontages => (root, root),
        }
    }

    /// Lowercase name (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RootMotionRoute::Ignore => "ignore",
            RootMotionRoute::Report => "report",
            RootMotionRoute::GraphAndMontages => "graph_and_montages",
            RootMotionRoute::OnlyMontages => "only_montages",
        }
    }
}

/// Drives skeletal animation on a rig entity (one that also has [`SkinnedMesh`]).
#[derive(Component, Clone, Debug)]
pub struct Animator {
    /// What plays: a graph (with its clip library) or a single looping/one-shot clip.
    pub source: AnimatorSource,
    /// Graph parameters (`animator.set("Speed", 3.2)`).
    pub parameters: ParameterSet,
    /// Global playback speed multiplier.
    pub speed: f32,
    /// Evaluation policy.
    pub update_mode: UpdateMode,
    /// Where extracted root motion goes (default: graph and montages both apply).
    pub root_motion_route: RootMotionRoute,

    // (runtime) playback instance matching `source`.
    pub(crate) instance: Option<GraphInstance>,
    // (runtime) evaluated local pose, written each evaluation.
    pub(crate) pose: Pose,
    // (runtime) pooled scratch poses (avoids per-frame allocation).
    pub(crate) scratch: Vec<Pose>,
    // (runtime) accumulator for LOD rate reduction.
    pub(crate) rate_accumulator: f32,
    // (runtime) animation seconds the last evaluation consumed (0 when this frame's evaluation
    // was gated off): secondary motion (springs, gaze) steps by this so it never covers a
    // different span of time than the pose it hangs from (audit-v040 M4, L10).
    pub(crate) last_eval_delta: f32,
    // (runtime) root motion of the last evaluation.
    pub(crate) last_root_motion: (Vec3, Quat),
    // (runtime) morph weights of the last evaluation.
    pub(crate) last_morphs: Vec<(u32, f32)>,
    // (runtime) per-node morph weights of the last evaluation: (node index, target, weight).
    pub(crate) last_node_morphs: Vec<(u32, u32, f32)>,
    // Declared cross-node sync groups and markers (`.sync.ron`, ADR-1465); `None` = off.
    pub(crate) sync_setup: Option<Arc<crate::sync::SyncSetup>>,
    // (runtime) sync-group leaders and clocks carried between evaluations.
    pub(crate) sync_state: crate::sync::SyncGroups,
    // (runtime) montage poses per slot for graph `Slot` nodes, reused between frames.
    pub(crate) slot_layers: crate::montage_player::SlotLayers,
    // (runtime) the graph's `Slot` node names and the player slot list the `Slot` nodes were
    // last checked against (`systems::slot_check_key`): a swapped graph, an in-place graph edit
    // or a changed slot list is checked again, and each combination warns once
    // (deep-zai-animation finding 5, rev-61 note 1).
    pub(crate) slots_checked_for: Option<u64>,
    // (runtime) curve sample buffer and recorded curve names, reused across evaluations.
    pub(crate) curve_sample: Vec<(String, f32)>,
    pub(crate) curves: Vec<(String, f32)>,
    // (runtime) node-morph sample buffer.
    pub(crate) node_raw: Vec<(u32, u32, f32)>,
    // (runtime) morph-list pools.
    pub(crate) morph_shared_pool: Vec<Vec<(u32, f32)>>,
    pub(crate) morph_nodes_pool: Vec<Vec<((u32, u32), f32)>>,
    // (runtime) IK chain matrices.
    pub(crate) ik_globals: Vec<Mat4>,
    // (runtime) `apply_animation` still owes the joints and the skin palette a write of `pose`.
    // A gated frame leaves this false, so apply does not rebuild the palette or rewrite joints.
    pub(crate) pose_dirty: bool,
    // (runtime) bind pose, curve names, mask weights and IK buffers reused across evaluations.
    pub(crate) reuse: EvalReuse,
}

/// What an [`Animator`] plays.
#[derive(Clone, Debug)]
pub enum AnimatorSource {
    /// A graph plus the clips it references.
    Graph {
        /// The graph asset (shared).
        graph: Arc<AnimGraph>,
        /// Clip lookup.
        clips: Arc<ClipLibrary>,
    },
    /// A single clip (played through an implicit one-clip graph).
    SingleClip {
        /// The clip.
        clip: Arc<AnimationClip>,
        /// Loop playback.
        looping: bool,
    },
}

impl Animator {
    /// An animator playing a single clip.
    pub fn single(clip: AnimationClip, looping: bool) -> Self {
        Animator {
            source: AnimatorSource::SingleClip {
                clip: Arc::new(clip),
                looping,
            },
            parameters: ParameterSet::new(),
            speed: 1.0,
            update_mode: UpdateMode::Always,
            root_motion_route: RootMotionRoute::default(),
            instance: None,
            pose: Pose::default(),
            scratch: Vec::new(),
            rate_accumulator: 0.0,
            last_eval_delta: 0.0,
            last_root_motion: (Vec3::ZERO, Quat::IDENTITY),
            last_morphs: Vec::new(),
            last_node_morphs: Vec::new(),
            sync_setup: None,
            sync_state: crate::sync::SyncGroups::default(),
            slot_layers: crate::montage_player::SlotLayers::default(),
            slots_checked_for: None,
            curve_sample: Vec::new(),
            curves: Vec::new(),
            node_raw: Vec::new(),
            morph_shared_pool: Vec::new(),
            morph_nodes_pool: Vec::new(),
            ik_globals: Vec::new(),
            pose_dirty: false,
            reuse: EvalReuse::default(),
        }
    }

    /// An animator playing a graph with the given clip library.
    pub fn graph(graph: AnimGraph, clips: ClipLibrary) -> Self {
        Animator {
            source: AnimatorSource::Graph {
                graph: Arc::new(graph),
                clips: Arc::new(clips),
            },
            parameters: ParameterSet::new(),
            speed: 1.0,
            update_mode: UpdateMode::Always,
            root_motion_route: RootMotionRoute::default(),
            instance: None,
            pose: Pose::default(),
            scratch: Vec::new(),
            rate_accumulator: 0.0,
            last_eval_delta: 0.0,
            last_root_motion: (Vec3::ZERO, Quat::IDENTITY),
            last_morphs: Vec::new(),
            last_node_morphs: Vec::new(),
            sync_setup: None,
            sync_state: crate::sync::SyncGroups::default(),
            slot_layers: crate::montage_player::SlotLayers::default(),
            slots_checked_for: None,
            curve_sample: Vec::new(),
            curves: Vec::new(),
            node_raw: Vec::new(),
            morph_shared_pool: Vec::new(),
            morph_nodes_pool: Vec::new(),
            ik_globals: Vec::new(),
            pose_dirty: false,
            reuse: EvalReuse::default(),
        }
    }

    /// Set a parameter by value kind: `animator.set("Speed", 3.2)`, `("Crouch", true)`,
    /// `("Aim", Vec3::ZERO)`, and so on.
    pub fn set(&mut self, name: &str, value: impl Into<ParamValue>) {
        match value.into() {
            ParamValue::Float(v) => self.parameters.set_float(name, v),
            ParamValue::Int(v) => self.parameters.set_int(name, v),
            ParamValue::Bool(v) => self.parameters.set_bool(name, v),
            ParamValue::Vec2(v) => self.parameters.set_vec2(name, v),
            ParamValue::Vec3(v) => self.parameters.set_vec3(name, v),
            ParamValue::Trigger => self.parameters.set_trigger(name),
        }
    }

    /// Declare cross-node sync groups and markers (`.sync.ron`; `docs/specs/17-animation.md`
    /// § 4b). `Clip` nodes whose `sync_group` names a declared group follow its leader; `None`
    /// turns cross-node syncing off again.
    pub fn set_sync_setup(&mut self, setup: Option<Arc<crate::sync::SyncSetup>>) {
        self.sync_setup = setup;
        self.sync_state = crate::sync::SyncGroups::default();
    }

    /// The declared sync setup, if any.
    pub fn sync_setup(&self) -> Option<&crate::sync::SyncSetup> {
        self.sync_setup.as_deref()
    }

    /// Sync groups that played in the last evaluation, with the leader chosen for the next.
    pub fn sync_groups(&self) -> &[crate::sync::SyncGroupState] {
        self.sync_state.groups()
    }

    /// Root motion `(translation, rotation)` of the last evaluation, rig object space.
    pub fn last_root_motion(&self) -> (Vec3, Quat) {
        self.last_root_motion
    }

    /// The local pose of the last evaluation (before IK sockets resolve and before any
    /// physics blend); empty until the first evaluation.
    pub fn evaluated_pose(&self) -> &Pose {
        &self.pose
    }

    /// The local pose of the last evaluation, to change it after [`evaluate_animators`] and
    /// before [`apply_animation`] (a sequence's clip tracks blend into it, ADR-1880): everything
    /// downstream follows, up to the joints and the skin palette. The next evaluation replaces it.
    ///
    /// [`evaluate_animators`]: crate::systems::evaluate_animators
    /// [`apply_animation`]: crate::systems::apply_animation
    pub fn evaluated_pose_mut(&mut self) -> &mut Pose {
        self.pose_dirty = true;
        &mut self.pose
    }

    /// Fire a trigger parameter (auto-resets when a transition consumes it).
    pub fn trigger(&mut self, name: &str) {
        self.parameters.set_trigger(name);
    }

    /// The graph, when this animator plays one.
    pub fn graph_def(&self) -> Option<&AnimGraph> {
        match &self.source {
            AnimatorSource::Graph { graph, .. } => Some(graph),
            AnimatorSource::SingleClip { .. } => None,
        }
    }

    /// The implicit graph of a single-clip animator: the clip node wrapped in an `Arc` shared
    /// with the source (built once by the evaluate system).
    pub(crate) fn ensure_instance(&mut self, joint_count: usize) {
        if self.instance.is_none() {
            let graph = match &self.source {
                AnimatorSource::Graph { graph, .. } => GraphInstance::from_node(&graph.root),
                AnimatorSource::SingleClip { .. } => GraphInstance::Clip(Default::default()),
            };
            self.instance = Some(graph);
            self.pose = Pose::from_identity(joint_count);
        }
    }

    /// A debug snapshot of the running graph (active nodes, weights, current states,
    /// transition progress and parameter values) for PIE debugging; `None` before the first
    /// evaluation or for single-clip animators.
    ///
    /// While IK debug is on ([`Self::set_ik_debug`]) the snapshot's `ik` rows report what
    /// each IK node reached on the latest evaluation.
    pub fn debug_snapshot(&self) -> Option<crate::debug::GraphDebug> {
        let graph = self.graph_def()?;
        let instance = self.instance.as_ref()?;
        Some(crate::debug::graph_debug_with_ik(
            graph,
            instance,
            &self.parameters,
            &self.reuse,
        ))
    }

    /// Turn per-node IK residual capture on or off for this rig (`anim.ik_report`,
    /// ADR-8708). Off by default; while off the evaluator does no IK debug work.
    pub fn set_ik_debug(&mut self, on: bool) {
        self.reuse.set_ik_debug(on);
    }

    /// Whether IK residual capture is on for this rig.
    #[must_use]
    pub fn ik_debug(&self) -> bool {
        self.reuse.ik_debug()
    }

    /// IK residual measurements this rig has made (saturating); 0 while capture was never on.
    #[must_use]
    pub fn ik_debug_captures(&self) -> u64 {
        self.reuse.ik_debug_captures()
    }

    /// Name of the current state, when the played graph's root (or an IK/root-motion wrapper
    /// around it) is a state machine. Useful for debugging and HUDs.
    pub fn current_state(&self) -> Option<String> {
        let graph = self.graph_def()?;
        match self.instance.as_ref()? {
            instance @ GraphInstance::StateMachine(_) => instance.state_name(root_machine(graph)),
            GraphInstance::Wrap(inner) | GraphInstance::Warp { inner, .. } => {
                inner.state_name(root_machine(graph))
            }
            _ => None,
        }
    }
}

/// The state-machine definition at (or near) a graph's root, if there is one.
fn root_machine(graph: &AnimGraph) -> &crate::state_machine::StateMachineDef {
    let inner = match &graph.root {
        crate::graph::AnimNode::StateMachine(sm) => return sm,
        crate::graph::AnimNode::TwoBoneIK(n) => &n.inner,
        crate::graph::AnimNode::LookAt(n) => &n.inner,
        crate::graph::AnimNode::RootMotion(n) => &n.inner,
        crate::graph::AnimNode::Slot(n) => &n.source,
        crate::graph::AnimNode::ChainIK(n) => &n.inner,
        crate::graph::AnimNode::BodyAim(n) => &n.inner,
        crate::graph::AnimNode::StrideWarp(n) => &n.inner,
        crate::graph::AnimNode::OrientationWarp(n) => &n.inner,
        _ => return placeholder_machine(),
    };
    match inner.as_ref() {
        crate::graph::AnimNode::StateMachine(sm) => sm,
        _ => placeholder_machine(),
    }
}

fn placeholder_machine() -> &'static crate::state_machine::StateMachineDef {
    // A static empty machine; `state_name` on it yields None.
    static EMPTY: std::sync::OnceLock<crate::state_machine::StateMachineDef> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| crate::state_machine::StateMachineDef {
        initial: String::new(),
        states: Vec::new(),
        transitions: Vec::new(),
    })
}

/// Binds a mesh entity (a rig) to a [`Skeleton`]: the skeleton (joint names, parents, bind
/// pose and inverse bind matrices) plus the joint entities in joint-id order (spec 17 § 2,
/// Tincal engine item C1).
///
/// Joint entities are ordinary entities, so sockets and attachments are children of a joint.
/// With an [`Animator`] on the same entity, the apply system writes each joint's local
/// `Transform` every frame and publishes the [`SkinPalette`]. Without one, the joints are posed
/// by whatever moves them (a glTF hierarchy, a pose sampler, physics), and
/// [`SkinnedMesh::palette_from_joint_worlds`] gives the palette from their world transforms.
/// `regia_framework::skinning::SkinnedMeshPlugin` hands either palette to the renderer.
///
/// This replaces `SkeletonBinding` (same fields; the old name is a deprecated alias).
#[derive(Component, Clone)]
pub struct SkinnedMesh {
    /// The skeleton asset (shared).
    pub skeleton: Arc<Skeleton>,
    /// Joint entities, indexed by joint id.
    pub joints: Vec<Entity>,
}

/// The old name of [`SkinnedMesh`] (the interim rig binding before C1). Same type: struct
/// literals, queries and `insert` calls keep working; rename them to `SkinnedMesh`.
#[deprecated(since = "0.2.0", note = "renamed to `SkinnedMesh` (Tincal C1); same fields")]
pub type SkeletonBinding = SkinnedMesh;

impl std::fmt::Debug for SkinnedMesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkinnedMesh")
            .field("joints", &self.joints.len())
            .finish_non_exhaustive()
    }
}

impl SkinnedMesh {
    /// A binding of `skeleton` to `joints` (joint-id order).
    #[must_use]
    pub fn new(skeleton: Arc<Skeleton>, joints: Vec<Entity>) -> Self {
        Self { skeleton, joints }
    }

    /// The entity of the joint named `name`.
    #[must_use]
    pub fn joint_entity(&self, name: &str) -> Option<Entity> {
        let index = self.skeleton.joint_index(name)?;
        self.joints.get(index as usize).copied()
    }

    /// Whether every skeleton joint has an entity.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.joints.len() == self.skeleton.len()
    }

    /// The skin palette (`mesh_world⁻¹ · joint_world · inverse_bind` per joint, joint-id
    /// order) from the joints' world matrices: `joint_world` gives each joint entity's world
    /// matrix. A mesh drawn with this palette at `mesh_world` puts each skinned vertex where its
    /// joints are. `None` when a joint has no world matrix, the binding is incomplete, or
    /// `mesh_world` cannot be inverted.
    #[must_use]
    pub fn palette_from_joint_worlds(
        &self,
        mesh_world: Mat4,
        joint_world: impl Fn(Entity) -> Option<Mat4>,
    ) -> Option<Vec<Mat4>> {
        if !self.is_complete() || mesh_world.determinant().abs() < f32::EPSILON {
            return None;
        }
        let to_mesh = mesh_world.inverse();
        self.joints
            .iter()
            .zip(&self.skeleton.inverse_bind_matrices)
            .map(|(&joint, inverse_bind)| joint_world(joint).map(|world| to_mesh * world * *inverse_bind))
            .collect()
    }
}

/// Object-space socket positions (joint global transform × offset) from the last applied
/// frame, keyed by `(rig, socket name)`; IK nodes target sockets through this map.
///
/// Object space is the rig's own frame (what [`JointGlobals`] holds), so a socket target does
/// not move when the rig is placed elsewhere in the world: the IK nodes solve there.
#[derive(Resource, Debug, Default)]
pub struct SocketTargets(pub HashMap<(Entity, String), Vec3>);

/// Attaches the entity to a joint of a rig: every frame (after animation applies) the
/// entity's world transform becomes `rig_world · joint_global · offset`, where `rig_world` is
/// the rig's `Transform` for a root rig and its (last propagated) `GlobalTransform` for a
/// parented one. The entity's `Transform` holds that world transform relative to its own
/// parent: a child of the rig gets `joint_global · offset`, an unparented entity the world
/// transform itself. A socket whose parent is itself a child entity composes against that
/// parent's last-propagated `GlobalTransform`, so it lags one frame while the parent moves —
/// the same rule as the rig's own placement. The socket's `name` is how IK nodes reference it
/// ([`crate::graph::IkTarget::Socket`]); glTF sockets import with the `socket_` prefix
/// stripped (`socket_turret` → `"turret"`).
#[derive(Component, Clone, Debug)]
pub struct Socket {
    /// The rig entity.
    pub rig: Entity,
    /// Socket name (used by IK targets and the socket-target map).
    pub name: String,
    /// Joint name in the rig's skeleton.
    pub joint: String,
    /// Local offset from the joint.
    pub offset: Transform,
}

/// CPU-side skinning output for the renderer: per joint `model_global · inverse_bind`,
/// in the rig's model space. The mesh world matrix takes a skinned vertex to world space
/// (`world = mesh_world · palette[j] · vertex`). The GPU skinning path (M5) consumes this.
#[derive(Component, Clone, Debug, Default)]
pub struct SkinPalette {
    /// One matrix per joint, joint-id order.
    pub matrices: Vec<Mat4>,
}

/// Object-space joint matrices of a rig from the last applied frame (used by sockets).
#[derive(Component, Clone, Debug, Default)]
pub struct JointGlobals {
    /// One matrix per joint, joint-id order (rig-local space).
    pub globals: Vec<Mat4>,
}

/// Morph (blend shape) weights written by animation; the renderer applies them. Defined in
/// `regia_scene` (so the renderer reads it without depending on animation) and re-exported
/// here, so `regia_animation::MorphWeights` keeps working.
pub use regia_scene::components::{MorphTargetNames, MorphWeights};

/// Root motion of a rig (spec 17 § 4), in the rig's object space.
///
/// `delta`/`rotation` are the last evaluation's motion (for HUDs and debugging). The
/// evaluate system also **accumulates** every evaluation into a pending sum that a consumer
/// drains with [`RootMotionOutput::take`]: the framework's `Character` does so in its fixed
/// step, so root motion is neither lost nor applied twice when render frames and fixed steps
/// do not line up one to one.
#[derive(Component, Clone, Copy, Debug)]
pub struct RootMotionOutput {
    /// Translation delta of the last evaluation in the rig's object space, metres.
    pub delta: Vec3,
    /// Rotation delta of the last evaluation.
    pub rotation: Quat,
    /// Accumulated translation since the last [`RootMotionOutput::take`] (object space at
    /// the start of the accumulation).
    pub pending_translation: Vec3,
    /// Accumulated rotation since the last take.
    pub pending_rotation: Quat,
    /// Animation time covered by the pending motion, seconds (positive in reverse playback
    /// too).
    pub pending_seconds: f32,
}

impl Default for RootMotionOutput {
    fn default() -> Self {
        RootMotionOutput {
            delta: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            pending_translation: Vec3::ZERO,
            pending_rotation: Quat::IDENTITY,
            pending_seconds: 0.0,
        }
    }
}

impl RootMotionOutput {
    /// Record one evaluation's motion: it becomes the last delta and is appended to the
    /// pending sum (the new delta is expressed in the frame the pending rotation reached).
    /// Motion nobody drained for [`RootMotionOutput::MAX_PENDING_SECONDS`] is dropped first,
    /// so a rig without a consumer does not accumulate forever.
    pub fn accumulate(&mut self, translation: Vec3, rotation: Quat, seconds: f32) {
        if self.pending_seconds > Self::MAX_PENDING_SECONDS {
            self.take();
        }
        self.delta = translation;
        self.rotation = rotation;
        self.pending_translation += self.pending_rotation * translation;
        self.pending_rotation = (self.pending_rotation * rotation).normalize();
        self.pending_seconds += seconds.max(0.0);
    }

    /// Undrained motion older than this (seconds) is dropped by the next accumulate.
    pub const MAX_PENDING_SECONDS: f32 = 1.0;

    /// Drain the pending motion: `(translation, rotation, seconds)` since the last take.
    pub fn take(&mut self) -> (Vec3, Quat, f32) {
        let out = (
            self.pending_translation,
            self.pending_rotation,
            self.pending_seconds,
        );
        self.pending_translation = Vec3::ZERO;
        self.pending_rotation = Quat::IDENTITY;
        self.pending_seconds = 0.0;
        out
    }
}

/// Spawn the joint entities for a skeleton and return the binding (the rig entity keeps its
/// own `Transform`, which positions the whole rig in the world).
pub fn spawn_skeleton(world: &mut World, skeleton: Arc<Skeleton>) -> SkinnedMesh {
    let mut joints = Vec::with_capacity(skeleton.len());
    for bind in &skeleton.bind_pose {
        let joint = world
            .spawn(Transform {
                translation: bind.translation,
                rotation: bind.rotation,
                scale: bind.scale,
            })
            .id();
        joints.push(joint);
    }
    SkinnedMesh { skeleton, joints }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_split_the_final_root_motion() {
        let root = (Vec3::new(0.0, 0.0, -1.0), Quat::from_rotation_y(0.3));
        let none = (Vec3::ZERO, Quat::IDENTITY);
        assert_eq!(RootMotionRoute::default(), RootMotionRoute::GraphAndMontages);
        assert_eq!(RootMotionRoute::GraphAndMontages.split(root), (root, root));
        assert_eq!(RootMotionRoute::OnlyMontages.split(root), (root, root));
        assert_eq!(RootMotionRoute::Report.split(root), (root, none));
        assert_eq!(RootMotionRoute::Ignore.split(root), (none, none));
    }

    #[test]
    fn only_montages_zeroes_the_graph_input() {
        let graph = (Vec3::X, Quat::from_rotation_y(0.5));
        assert_eq!(RootMotionRoute::GraphAndMontages.graph_input(graph), graph);
        assert_eq!(RootMotionRoute::Report.graph_input(graph), graph);
        assert_eq!(
            RootMotionRoute::OnlyMontages.graph_input(graph),
            (Vec3::ZERO, Quat::IDENTITY)
        );
    }

    #[test]
    fn root_motion_accumulates_in_the_turning_frame_and_drains_once() {
        let mut out = RootMotionOutput::default();
        let quarter = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        // Step forward (−Z), turn 90° left, step forward again: ends 1 m forward, 1 m left.
        out.accumulate(Vec3::new(0.0, 0.0, -1.0), quarter, 0.1);
        out.accumulate(Vec3::new(0.0, 0.0, -1.0), Quat::IDENTITY, 0.1);
        let (translation, rotation, seconds) = out.take();
        assert!(
            (translation - Vec3::new(-1.0, 0.0, -1.0)).length() < 1e-5,
            "{translation}"
        );
        assert!(rotation.angle_between(quarter) < 1e-5);
        assert!((seconds - 0.2).abs() < 1e-6);
        assert_eq!(out.take().0, Vec3::ZERO, "drained");
        assert_eq!(out.delta, Vec3::new(0.0, 0.0, -1.0), "last delta stays readable");
    }

    #[test]
    fn undrained_root_motion_is_capped() {
        let mut out = RootMotionOutput::default();
        for _ in 0..200 {
            out.accumulate(Vec3::X, Quat::IDENTITY, 0.016);
        }
        assert!(out.pending_seconds <= RootMotionOutput::MAX_PENDING_SECONDS + 0.02);
        assert!(out.pending_translation.x <= 64.0);
    }
}
