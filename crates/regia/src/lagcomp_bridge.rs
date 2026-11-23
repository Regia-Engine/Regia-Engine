// SPDX-License-Identifier: MPL-2.0
//! Lag compensation wired to the engine's physics, animation and gizmos (spec 75 §§ 4, 5, 11; spec
//! 90 § 4 (b), (c), (h)).
//!
//! `regia_net` has no edge to those crates, so [`LagCompBridgePlugin`] lives here, next to the
//! other adapters: it installs [`physics_occlusion`] as the static-world test, [`animator_joint`]
//! as the joint pose source (hosts that set their own hook keep it), and draws the `net_lagcomp`
//! gizmo group. Add it after `regia_net::LagCompPlugin`.
//!
//! With [`LagCompBridgePlugin::exact_tick_joints`] it also installs [`animator_state`]
//! (ADR-2551): the history keeps each `PoseSource::Joints` rig's animation state per tick and a
//! rewind evaluates the bone-attached shapes at the exact server tick through
//! `regia_animation::pose_eval`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use regia_animation::components::{Animator, AnimatorSource, SkinnedMesh};
use regia_animation::montage_player::MontagePlayer;
use regia_animation::params::ParameterSet;
use regia_animation::pose::Pose;
use regia_animation::pose_eval::{
    AnimStateSnapshot, ParamLanes, ParamLayout, PoseRequest, bind_chain_reach, capture_state, evaluate_pose,
    next_eval_delta,
};
use regia_animation::skeleton::Skeleton;
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Ray};
#[cfg(feature = "client")]
use regia_core::math::{Color, Isometry, Vec3};
#[cfg(feature = "client")]
use regia_net::lagcomp::{
    HitShapeKind, LagCompEchoLog, LagCompHistory, PosedShape, QueryRecord, ShapeReport,
};
use regia_net::lagcomp::{
    LagCompAnimSource, LagCompJointSource, LagCompOcclusion, RecordedAnim, install_lag_compensation,
    root_pose,
};
use regia_physics::{PhysicsLayers, PhysicsWorld, QueryFilter, WorldQueries};
#[cfg(feature = "client")]
use regia_render::gizmos::{
    AppGizmoBuilder, GizmoBuffer, GizmoConfig, GizmoConfigGroup, GizmoConfigStore, Gizmos,
    gizmo_group_enabled,
};
use regia_scene::Transform;
use regia_scene::transform::GlobalTransform;

/// Ancestors walked looking for an ignored owner of a collider.
const OWNER_WALK_LIMIT: usize = 64;

/// Descendants searched for the entity carrying the rig.
const RIG_SEARCH_LIMIT: usize = 64;

/// The rewound shapes of a query.
#[cfg(feature = "client")]
const REWOUND: Color = Color::rgb(0.2, 0.9, 0.3);
/// The same entity's shapes on the newest recorded tick.
#[cfg(feature = "client")]
const CURRENT: Color = Color::rgb(0.55, 0.55, 0.55);
/// The ray.
#[cfg(feature = "client")]
const RAY: Color = Color::rgb(0.95, 0.85, 0.2);
/// The hit point.
#[cfg(feature = "client")]
const HIT: Color = Color::rgb(0.95, 0.2, 0.2);
/// A rewound shape the server echoed to this client.
#[cfg(feature = "client")]
const ECHO: Color = Color::rgb(0.9, 0.3, 0.9);

/// Gizmo group `net_lagcomp`: for the last 16 queries the rewound shapes (green), the same entity
/// on the newest recorded tick (grey), the ray and the hit point; on a client, the echoed shapes
/// (magenta). Off by default; `debug.set_gizmos` turns it on.
#[cfg(feature = "client")]
#[derive(Default, Debug, Clone, Copy)]
pub struct NetLagCompGizmos;

#[cfg(feature = "client")]
impl GizmoConfigGroup for NetLagCompGizmos {
    const NAME: &'static str = "net_lagcomp";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..GizmoConfig::default()
        }
    }
}

/// Which colliders occlude a shot: with `Some(mask)` only those on the layers of the bitmask
/// (`1 << layer_index`), so bullets can pass through glass. `None` tests every solid collider.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagCompOcclusionLayers(pub Option<u32>);

/// See the module docs.
#[derive(Default, Debug, Clone, Copy)]
pub struct LagCompBridgePlugin {
    /// [`LagCompOcclusionLayers`].
    pub occlusion_layers: Option<u32>,
    /// Record `PoseSource::Joints` rigs as animation state and pose their joints at the exact
    /// rewound tick ([`animator_state`], ADR-2551). Off by default: the history then keeps
    /// posed shapes from the last evaluated pose, as before.
    pub exact_tick_joints: bool,
}

impl Plugin for LagCompBridgePlugin {
    fn build(&self, app: &mut App) {
        install_lag_compensation(app.world_mut());
        app.insert_resource(LagCompOcclusionLayers(self.occlusion_layers));
        let world = app.world_mut();
        let mut occlusion = world.resource_mut::<LagCompOcclusion>();
        occlusion.0 = occlusion.0.or(Some(physics_occlusion));
        let mut joints = world.resource_mut::<LagCompJointSource>();
        joints.0 = joints.0.or(Some(animator_joint));
        if self.exact_tick_joints {
            let mut anim = world.resource_mut::<LagCompAnimSource>();
            anim.0 = anim.0.or(Some(animator_state));
        }
    }

    #[cfg(feature = "client")]
    fn finish(&self, app: &mut App) {
        // A dedicated server has no renderer and nothing to draw on.
        if !app.world().contains_resource::<GizmoConfigStore>() {
            return;
        }
        app.init_gizmo_group::<NetLagCompGizmos>();
        app.add_systems(
            PostUpdate,
            draw_lagcomp_gizmos
                .after(PostUpdateSet::TransformPropagate)
                .run_if(gizmo_group_enabled::<NetLagCompGizmos>),
        );
    }
}

/// Whether `entity` or an ancestor is in `ignore` (sorted).
fn owned_by(world: &World, entity: Entity, ignore: &[Entity]) -> bool {
    let mut current = Some(entity);
    for _ in 0..OWNER_WALK_LIMIT {
        let Some(e) = current else {
            return false;
        };
        if ignore.binary_search(&e).is_ok() {
            return true;
        }
        current = world.get::<ChildOf>(e).map(ChildOf::parent);
    }
    false
}

/// The occlusion hook: the nearest solid collider along the ray in the present physics world.
/// Sensors never occlude; neither do the colliders of the ignored entities (every lag-compensated
/// entity and the shooter's own) or their children, whose present positions are not where they
/// were at the rewound tick. Without a [`PhysicsWorld`] nothing occludes.
pub fn physics_occlusion(world: &World, ray: &Ray, max_dist: f32, ignore: &[Entity]) -> Option<f32> {
    let queries = WorldQueries {
        world: world.get_resource::<PhysicsWorld>()?,
        layers: world.get_resource::<PhysicsLayers>(),
    };
    let allowed = |collider: Entity| !owned_by(world, collider, ignore);
    let mut filter = QueryFilter::new().exclude_sensors().predicate(&allowed);
    if let Some(mask) = world.get_resource::<LagCompOcclusionLayers>().and_then(|l| l.0) {
        filter = filter.layers(mask);
    }
    queries
        .raycast(ray.origin, ray.direction, max_dist, filter)
        .map(|h| h.distance)
}

/// Joint `joint` in the skeleton's object space from `pose` (the bind pose for joints it lacks).
fn object_space_joint(skeleton: &Skeleton, pose: &Pose, joint: usize) -> Affine3A {
    let local = |i: usize| match (
        pose.translations.get(i),
        pose.rotations.get(i),
        pose.scales.get(i),
    ) {
        (Some(t), Some(r), Some(s)) => Affine3A::from_scale_rotation_translation(*s, *r, *t),
        _ => skeleton.bind_pose.get(i).map_or(Affine3A::IDENTITY, |b| {
            Affine3A::from_scale_rotation_translation(b.scale, b.rotation, b.translation)
        }),
    };
    let mut out = local(joint);
    let mut parent = skeleton.parents.get(joint).copied().flatten();
    // Bounded: skeleton data must not be able to spin a server forever.
    for _ in 0..skeleton.parents.len() {
        let Some(p) = parent else {
            break;
        };
        out = local(p as usize) * out;
        parent = skeleton.parents.get(p as usize).copied().flatten();
    }
    out
}

/// The entity carrying the rig for `root`: `root` itself when it has an [`Animator`] and a
/// [`SkinnedMesh`], otherwise the nearest such descendant (at most 64 descendants away).
///
/// Resolving a rig that sits on `root` does not allocate.
pub fn rig_entity(world: &World, root: Entity) -> Option<Entity> {
    rig_of(world, root)
}

/// A breadth-first walk of at most 64 descendants, stored on the stack.
struct RigWalk {
    ring: [Entity; RIG_SEARCH_LIMIT],
    head: usize,
    tail: usize,
    /// Entities currently in the ring.
    count: usize,
    /// Descendants queued so far, including ones already popped.
    queued: usize,
}

impl RigWalk {
    fn new(root: Entity) -> Self {
        Self {
            ring: [root; RIG_SEARCH_LIMIT],
            head: 0,
            tail: 0,
            count: 0,
            queued: 0,
        }
    }

    /// Queues `child` until 64 descendants have been queued. Returns false when the cap is full.
    fn push(&mut self, child: Entity) -> bool {
        if self.queued == RIG_SEARCH_LIMIT {
            return false;
        }
        self.ring[self.tail] = child;
        self.tail += 1;
        if self.tail == RIG_SEARCH_LIMIT {
            self.tail = 0;
        }
        self.count += 1;
        self.queued += 1;
        true
    }

    fn pop(&mut self) -> Option<Entity> {
        if self.count == 0 {
            return None;
        }
        let entity = self.ring[self.head];
        self.head += 1;
        if self.head == RIG_SEARCH_LIMIT {
            self.head = 0;
        }
        self.count -= 1;
        Some(entity)
    }
}

/// Copies `children` into the walk, stopping at the descendant cap.
fn enqueue(walk: &mut RigWalk, children: &Children) {
    for child in children.iter() {
        if !walk.push(child) {
            break;
        }
    }
}

/// The entity carrying the rig: `root`, or the nearest descendant with an [`Animator`].
///
/// The walk is a 64-slot ring on the stack. A rig on `root` returns before that ring exists, so
/// the lookup does not allocate. Descendants past the cap are not queued (a cycle stops).
fn rig_of(world: &World, root: Entity) -> Option<Entity> {
    let mut walk = {
        let root_ent = world.get_entity(root).ok()?;
        if root_ent.contains::<Animator>() && root_ent.contains::<SkinnedMesh>() {
            return Some(root);
        }
        let mut walk = RigWalk::new(root);
        if let Some(children) = root_ent.get::<Children>() {
            enqueue(&mut walk, children);
        }
        walk
    };
    while let Some(entity) = walk.pop() {
        // A missing link aborts the walk, as the heap queue did.
        let ent = world.get_entity(entity).ok()?;
        if ent.contains::<Animator>() && ent.contains::<SkinnedMesh>() {
            return Some(entity);
        }
        if let Some(children) = ent.get::<Children>() {
            enqueue(&mut walk, children);
        }
    }
    None
}

/// The joint pose hook: `joint` of the rig on `root` (or under it) from its animator's last
/// evaluated pose, placed under the root's fresh transform. `None` without a rig or such a joint,
/// and the recorder falls back to the scene hierarchy. The pose is as fresh as the last animation
/// evaluation (spec 90 § 4 (c)).
pub fn animator_joint(world: &World, root: Entity, joint: &str) -> Option<Affine3A> {
    let rig = rig_of(world, root)?;
    let ent = world.get_entity(rig).ok()?;
    let animator = ent.get::<Animator>()?;
    let skinned = ent.get::<SkinnedMesh>()?;
    let index = skinned.skeleton.joint_index(joint)? as usize;
    let object = object_space_joint(&skinned.skeleton, animator.evaluated_pose(), index);
    let root_ref = world.get_entity(root).ok()?;
    let root_now = root_pose(world, &root_ref);
    if rig == root {
        return Some(root_now * object);
    }
    // The rig's pose relative to the root from fresh locals: a `GlobalTransform` can be a frame
    // old inside the fixed step. A link without a `Transform` falls back to the globals.
    let relative = local_chain(world, root, rig).or_else(|| {
        let root_global = root_ref.get::<GlobalTransform>()?.0;
        let rig_global = ent.get::<GlobalTransform>()?.0;
        Some(root_global.inverse() * rig_global)
    })?;
    Some(root_now * relative * object)
}

/// The pose of `rig` relative to its ancestor `root`: the `Transform`s from the rig up to, but not
/// including, the root. `None` when a link has no `Transform` or the walk does not reach the root.
fn local_chain(world: &World, root: Entity, rig: Entity) -> Option<Affine3A> {
    let mut out = Affine3A::IDENTITY;
    let mut at = rig;
    for _ in 0..=RIG_SEARCH_LIMIT {
        if at == root {
            return Some(out);
        }
        let ent = world.get_entity(at).ok()?;
        let t = ent.get::<Transform>()?;
        out = Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation) * out;
        at = ent.get::<ChildOf>()?.parent();
    }
    None
}

/// Scratch reused across pose queries of one rig. It lives on `RigShared`, not on each tick, so
/// it stays out of the per-tick byte budget.
#[derive(Debug, Default)]
struct PoseScratch {
    /// Parameter set built once, then updated in place by `ParamLayout::apply`.
    params: ParameterSet,
    params_ready: bool,
    pose: Pose,
    /// Object-space joint globals from the same composition as `object_space_joint`.
    globals: Vec<Affine3A>,
    /// First matching joint name, the same answer as `Skeleton::joint_index`.
    names: HashMap<String, u32>,
    names_ready: bool,
    cached_params: Box<[u32]>,
    cached_words: Box<[u32]>,
    cached_delta_bits: u32,
    valid: bool,
}

/// What stays the same between ticks of one rig: shared by its records.
#[derive(Debug)]
struct RigShared {
    skeleton: Arc<Skeleton>,
    source: AnimatorSource,
    layout: Arc<ParamLayout>,
    rig: Entity,
    /// The rig's pose relative to the root.
    relative: Affine3A,
    /// [`RecordedAnim::reach`].
    reach: f32,
    /// Pose evaluation scratch. One slot, keyed by the tick's inputs.
    scratch: Mutex<PoseScratch>,
}

impl RigShared {
    fn same_source(&self, source: &AnimatorSource) -> bool {
        match (&self.source, source) {
            (AnimatorSource::Graph { graph: a, clips: c }, AnimatorSource::Graph { graph: b, clips: d }) => {
                Arc::ptr_eq(a, b) && Arc::ptr_eq(c, d)
            }
            (
                AnimatorSource::SingleClip { clip: a, looping: la },
                AnimatorSource::SingleClip { clip: b, looping: lb },
            ) => Arc::ptr_eq(a, b) && la == lb,
            _ => false,
        }
    }
}

/// One rig's animation state on one tick ([`animator_state`]).
#[derive(Debug)]
struct RigTick {
    shared: Arc<RigShared>,
    state: AnimStateSnapshot,
    params: Box<[u32]>,
    /// Animation seconds of the tick's evaluation (the animator's speed applied).
    delta: f32,
}

/// True when `scratch` already holds the pose for these inputs.
fn pose_cache_hit(scratch: &PoseScratch, params: &[u32], words: &[u32], delta_bits: u32) -> bool {
    scratch.valid
        && scratch.cached_delta_bits == delta_bits
        && &*scratch.cached_params == params
        && &*scratch.cached_words == words
}

/// Remembers the inputs that produced the scratch pose. Same-sized keys are copied in place.
fn remember_pose(scratch: &mut PoseScratch, params: &[u32], words: &[u32], delta_bits: u32) {
    fn copy_into(slot: &mut Box<[u32]>, src: &[u32]) {
        if slot.len() == src.len() {
            slot.copy_from_slice(src);
        } else {
            *slot = src.to_vec().into_boxed_slice();
        }
    }
    copy_into(&mut scratch.cached_params, params);
    copy_into(&mut scratch.cached_words, words);
    scratch.cached_delta_bits = delta_bits;
    scratch.valid = true;
}

/// Object-space joint globals. `Skeleton::globals` is a `Mat4` pass with no bind fallback, so
/// this calls `object_space_joint` and stays bit-identical with the live joint hook.
fn fill_object_globals(skeleton: &Skeleton, pose: &Pose, globals: &mut Vec<Affine3A>) {
    globals.clear();
    globals.reserve(skeleton.len());
    for index in 0..skeleton.len() {
        globals.push(object_space_joint(skeleton, pose, index));
    }
}

/// Builds the joint-name map once. Valid skeletons have unique names; the first match wins, as
/// `Skeleton::joint_index` does.
fn ensure_joint_names(skeleton: &Skeleton, scratch: &mut PoseScratch) {
    if scratch.names_ready {
        return;
    }
    scratch.names.clear();
    for (index, name) in skeleton.joint_names.iter().enumerate() {
        scratch.names.entry(name.clone()).or_insert(index as u32);
    }
    scratch.names_ready = true;
}

impl RecordedAnim for RigTick {
    fn joint_poses(&self, root: &Affine3A, joints: &[&str], out: &mut Vec<Option<Affine3A>>) -> bool {
        let _span = tracing::trace_span!("net_lagcomp_pose_eval").entered();
        out.clear();
        let mut scratch = self
            .shared
            .scratch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let words = self.state.words();
        let delta_bits = self.delta.to_bits();
        if !pose_cache_hit(&scratch, &self.params, words, delta_bits) {
            if scratch.params_ready {
                self.shared.layout.apply(&self.params, &mut scratch.params);
            } else {
                scratch.params = self.shared.layout.decode(&self.params);
                scratch.params_ready = true;
            }
            let evaluated = {
                // Split the scratch so the request can borrow the parameter set while the pose
                // is written.
                let PoseScratch { params, pose, .. } = &mut *scratch;
                let request = PoseRequest {
                    skeleton: &self.shared.skeleton,
                    source: &self.shared.source,
                    state: &self.state,
                    params,
                    delta: self.delta,
                    rig: self.shared.rig,
                    socket_targets: None,
                };
                if pose.len() != self.shared.skeleton.len() {
                    *pose = self.shared.skeleton.bind_pose_pose();
                }
                evaluate_pose(&request, pose).is_ok()
            };
            if !evaluated {
                scratch.valid = false;
                return false;
            }
            let PoseScratch { pose, globals, .. } = &mut *scratch;
            fill_object_globals(&self.shared.skeleton, pose, globals);
            remember_pose(&mut scratch, &self.params, words, delta_bits);
        }
        ensure_joint_names(&self.shared.skeleton, &mut scratch);
        let base = *root * self.shared.relative;
        for name in joints {
            let posed = scratch
                .names
                .get(*name)
                .copied()
                .and_then(|index| scratch.globals.get(index as usize).map(|global| base * *global));
            out.push(posed);
        }
        true
    }

    fn has_joint(&self, joint: &str) -> bool {
        self.shared.skeleton.joint_index(joint).is_some()
    }

    fn reach(&self) -> f32 {
        self.shared.reach
    }

    fn heap_bytes(&self) -> usize {
        // The record's own allocation (the `Arc` counts and the struct) plus its slices.
        2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<Self>()
            + self.state.heap_bytes()
            + std::mem::size_of_val(&*self.params)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Slack on the bind-pose chain reach for translation tracks (hips bob, crouch, a stretched arm).
const REACH_SLACK: f32 = 1.5;

/// The exact-tick capture hook ([`LagCompAnimSource`], ADR-2551): the playback state and
/// parameters of the rig on `root` (or its nearest rigged descendant), and the delta of one fixed
/// step. Reads the world only. The codes of a refusal: `no_rig`, `montage_active` (a montage
/// blends after the graph and is not part of the state), and the `anim_*` codes of
/// `regia_animation::pose_eval::CaptureError`.
pub fn animator_state(
    world: &World,
    root: Entity,
    seconds: f32,
    previous: Option<&Arc<dyn RecordedAnim>>,
) -> Result<Arc<dyn RecordedAnim>, &'static str> {
    let rig = rig_of(world, root).ok_or("no_rig")?;
    let ent = world.get_entity(rig).map_err(|_| "no_rig")?;
    let animator = ent.get::<Animator>().ok_or("no_rig")?;
    let skinned = ent.get::<SkinnedMesh>().ok_or("no_rig")?;
    if ent.get::<MontagePlayer>().is_some_and(MontagePlayer::is_active) {
        return Err("montage_active");
    }
    let state = capture_state(animator).map_err(|e| e.code())?;
    let relative = if rig == root {
        Affine3A::IDENTITY
    } else {
        local_chain(world, root, rig)
            .or_else(|| {
                let root_global = world.get::<GlobalTransform>(root)?.0;
                let rig_global = ent.get::<GlobalTransform>()?.0;
                Some(root_global.inverse() * rig_global)
            })
            .ok_or("no_rig")?
    };
    let previous = previous
        .and_then(|p| p.as_any().downcast_ref::<RigTick>())
        .map(|p| &p.shared);
    let lanes = ParamLanes::capture(&animator.parameters, previous.map(|p| &p.layout));
    let shared = match previous {
        Some(p)
            if Arc::ptr_eq(&p.skeleton, &skinned.skeleton)
                && p.same_source(&animator.source)
                && Arc::ptr_eq(&p.layout, &lanes.layout)
                && p.rig == rig
                && p.relative == relative =>
        {
            p.clone()
        }
        _ => {
            let chain = bind_chain_reach(&skinned.skeleton)
                .into_iter()
                .fold(0.0, f32::max);
            let (scale, _, offset) = relative.to_scale_rotation_translation();
            Arc::new(RigShared {
                skeleton: skinned.skeleton.clone(),
                source: animator.source.clone(),
                layout: lanes.layout.clone(),
                rig,
                relative,
                reach: chain * REACH_SLACK * scale.abs().max_element().max(1e-6) + offset.length(),
                scratch: Mutex::new(PoseScratch::default()),
            })
        }
    };
    Ok(Arc::new(RigTick {
        shared,
        state,
        params: lanes.values,
        delta: next_eval_delta(animator, seconds),
    }))
}

/// Draws one posed shape.
#[cfg(feature = "client")]
pub fn draw_shape(buffer: &mut GizmoBuffer<NetLagCompGizmos>, shape: &PosedShape, color: Color) {
    let at = Isometry {
        rotation: shape.rotation,
        translation: shape.position,
    };
    match shape.kind {
        HitShapeKind::Sphere { radius } => {
            buffer.sphere(at, radius, color);
        }
        HitShapeKind::Capsule { radius, half_height } => {
            buffer.capsule(at, radius, half_height, color);
        }
        HitShapeKind::Box { half_extents } => buffer.cuboid(at, half_extents, color),
    }
}

/// Draws logged queries; `current` gives a hit entity's shapes on the newest recorded tick.
#[cfg(feature = "client")]
pub fn draw_queries(
    buffer: &mut GizmoBuffer<NetLagCompGizmos>,
    queries: &[QueryRecord],
    current: impl Fn(u64) -> Vec<ShapeReport>,
) {
    for q in queries {
        let origin = Vec3::from_array(q.origin);
        let direction = Vec3::from_array(q.direction);
        let Some(hit) = &q.hit else {
            buffer.ray(origin, direction * 100.0, RAY);
            continue;
        };
        let point = Vec3::from_array(hit.point);
        buffer.line(origin, point, RAY);
        buffer.cross(point, 0.05, HIT);
        for s in q.shapes.iter().filter_map(ShapeReport::posed) {
            draw_shape(buffer, &s, REWOUND);
        }
        for s in current(hit.entity).iter().filter_map(ShapeReport::posed) {
            draw_shape(buffer, &s, CURRENT);
        }
    }
}

/// `PostUpdate`: the `net_lagcomp` group (a query's shapes on the server or the listen host, the
/// echoed shapes on a client). Runs only while the group is on.
#[cfg(feature = "client")]
fn draw_lagcomp_gizmos(
    history: Option<Res<LagCompHistory>>,
    echoes: Option<Res<LagCompEchoLog>>,
    mut gizmos: Gizmos<NetLagCompGizmos>,
) {
    let _span = tracing::trace_span!("net_lagcomp_gizmos").entered();
    if let Some(history) = history {
        draw_queries(&mut gizmos, &history.last_queries(), |bits| {
            Entity::try_from_bits(bits)
                .and_then(|e| history.newest_shapes_of(e))
                .unwrap_or_default()
        });
    }
    if let Some(echoes) = echoes {
        for shape in echoes.iter().flat_map(|e| e.shapes.iter()) {
            draw_shape(&mut gizmos, &shape.posed(), ECHO);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn rig(world: &mut World) -> (Animator, SkinnedMesh) {
        let skeleton = Arc::new(
            Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![regia_animation::skeleton::LocalBind::default()],
            )
            .expect("valid skeleton"),
        );
        let binding = regia_animation::components::spawn_skeleton(world, skeleton);
        (
            Animator::single(regia_animation::clip::AnimationClip::empty("c", 1.0), true),
            binding,
        )
    }

    /// A root and a chain of `depth` descendants; the rig is the last one.
    fn chain(depth: usize) -> (World, Entity, Entity) {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        let mut at = root;
        for _ in 0..depth {
            at = world.spawn((Transform::default(), ChildOf(at))).id();
        }
        let (animator, skinned) = rig(&mut world);
        world.entity_mut(at).insert((animator, skinned));
        (world, root, at)
    }

    #[test]
    fn the_descendant_cap_does_not_count_the_root() {
        let (world, root, last) = chain(RIG_SEARCH_LIMIT);
        assert_eq!(
            rig_of(&world, root),
            Some(last),
            "the 64th descendant is searched"
        );
        let (world, root, _) = chain(RIG_SEARCH_LIMIT + 1);
        assert_eq!(rig_of(&world, root), None, "the 65th is past the cap");
    }

    #[test]
    fn a_root_that_carries_the_rig_is_the_rig() {
        let mut world = World::new();
        let (animator, skinned) = rig(&mut world);
        let root = world.spawn((Transform::default(), animator, skinned)).id();
        assert_eq!(rig_of(&world, root), Some(root));
    }

    /// A root with `count` direct children; the rig is the last one.
    fn wide(count: usize) -> (World, Entity, Entity) {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        let mut last = root;
        for _ in 0..count {
            last = world.spawn((Transform::default(), ChildOf(root))).id();
        }
        let (animator, skinned) = rig(&mut world);
        world.entity_mut(last).insert((animator, skinned));
        (world, root, last)
    }

    #[test]
    fn a_wide_root_finds_its_64th_child_and_not_the_65th() {
        let (world, root, last) = wide(RIG_SEARCH_LIMIT);
        assert_eq!(rig_of(&world, root), Some(last), "the 64th child is searched");
        let (world, root, _) = wide(RIG_SEARCH_LIMIT + 1);
        assert_eq!(rig_of(&world, root), None, "the 65th child is past the cap");
    }
}
