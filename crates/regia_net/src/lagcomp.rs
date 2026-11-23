// SPDX-License-Identifier: MPL-2.0
//! Lag compensation for hit checks (spec 75, build record spec 90, ADR-1752).
//!
//! The server keeps, for every lag-compensated entity, the world pose of each hit shape on each of
//! the last few server ticks. A shot is then tested against the shapes as the shooter saw them, at
//! the view tick the shooter reported (spec 74 § 8, [`NetViewTick`]).
//!
//! | Item | Role |
//! |---|---|
//! | [`HitShapes`] | the shapes of an entity (sphere, capsule, box), on its root or on a joint |
//! | [`LagCompHistory`] | the server ring of recorded ticks, counters and the query log |
//! | [`lagcomp_record`] | records one tick (`NetSet::LagCompRecord`, end of `FixedUpdate`) |
//! | [`LagCompensation`] | rewound `raycast`, `sphere_cast`, `overlap_sphere`, `validate_claim` |
//! | [`RewoundView`] | the same queries at an explicit tick (projectiles, replays) |
//! | [`HitClaim`] | a client's "I hit X" message the server validates |
//! | [`LagCompOcclusion`], [`LagCompJointSource`] | host hooks: static-world occlusion, joint poses |
//!
//! Queries never write a component and never move an entity: they read the history and, for the
//! present tick, the live world. Candidates are visited in `NetId` order and the nearest hit wins
//! (ties by `NetId`), so results do not depend on archetype order.

pub mod anim;
pub mod echo;
pub mod geom;
pub mod window;

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use glam::{Affine3A, Quat, Vec3};
use regia_app::{App, FixedTime, FixedUpdate, Plugin};
use regia_core::math::{Aabb, Ray};
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::prelude::*;
use regia_scene::components::Name;
use regia_scene::transform::{GlobalTransform, Transform};
use serde::Serialize;
use thiserror::Error;

use crate::interp::NetViewTick;
use crate::message::{Direction, NetMessage, ToServerMessage, ValidateContext, Verdict};
use crate::model::{
    NetId, NetMode, NetOwner, NetPlayer, NetPlayerId, NetTick, NetTickFrac, OwnerCache, OwnerResolution,
    PeerId,
};
use crate::plugin::NetSet;
use crate::replication::NetGroups;
use crate::runtime::NetNode;
use crate::settings::{LagCompSettings, NetSettings, RateLimit};

pub use anim::{AnimCaptureFn, LagCompAnimSource, RecordedAnim};
pub use echo::{EchoShape, LagCompEcho, LagCompEchoLog, LagCompEchoPlugin};
pub use geom::{PosedShape, ShapeHit};
pub use window::{
    INTERP_JITTER_FACTOR, LagCompWindows, MAX_REWIND_CEILING_MS, ShooterWindow, VIEW_JITTER_FACTOR,
    VIEW_SLACK_MS, set_shooter_rtt,
};

/// Queries kept in the [`LagCompHistory::last_queries`] ring.
pub const QUERY_LOG_LEN: usize = 16;

/// Hits waiting for [`echo::lagcomp_echo_send`] at most (the oldest go first): the most a tick echoes.
const ECHO_QUEUE_LEN: usize = 8;

/// Descendants searched for a named joint before giving up.
const JOINT_SEARCH_LIMIT: usize = 512;

/// Ancestors composed into an entity's pose: a malformed hierarchy must not spin forever.
const PARENT_WALK_LIMIT: usize = 64;

/// Quaternion dot of a 45° turn (`cos 22.5°`). Up to that turn between two ticks a blended pose
/// stays inside the two recorded boxes; past it a corner can stick out of both (see `Entry::swing`).
const SWING_DOT: f32 = 0.923_88;

/// Edge of one lag-compensation broad-phase cell, in metres.
const LAGCOMP_CELL: f32 = 8.0;

/// An entity or a query that spans more cells than this on one axis is not gridded.
const CELL_SPAN_MAX: i32 = 24;

// ---- components ---------------------------------------------------------------------------------

/// Server and clients: this entity's hit shapes (spec 75 § 3). Reflected, saved in prefabs. Sizes
/// are in metres; the entity's scale does not scale them.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct HitShapes {
    /// The shapes (at most `lag_compensation.max_shapes_per_entity` are recorded).
    pub shapes: Vec<HitShape>,
    /// What joint-attached shapes follow.
    pub pose_source: PoseSource,
}

/// One hit shape.
#[derive(Reflect, Clone, Debug, PartialEq)]
pub struct HitShape {
    /// The shape and its size.
    pub kind: HitShapeKind,
    /// What it is attached to.
    pub attach: HitAttach,
    /// Placement relative to the attach point (scale ignored).
    pub offset: Transform,
    /// Free-form label returned with hits (`head`, `body`, `limb`); damage rules read it.
    pub tag: Name,
}

impl HitShape {
    /// A shape on the entity root, centered on it.
    pub fn new(kind: HitShapeKind, tag: &str) -> Self {
        Self {
            kind,
            attach: HitAttach::Root,
            offset: Transform::default(),
            tag: Name::new(tag),
        }
    }

    /// The same shape moved by `translation` from its attach point.
    #[must_use]
    pub fn at(mut self, translation: Vec3) -> Self {
        self.offset.translation = translation;
        self
    }

    /// The same shape attached to the joint `name`.
    #[must_use]
    pub fn on_joint(mut self, name: &str) -> Self {
        self.attach = HitAttach::Joint(Name::new(name));
        self
    }
}

/// The geometry of a hit shape. A capsule's axis is its local Y and `half_height` is half the
/// distance between its cap centers.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, crate::Replicate)]
pub enum HitShapeKind {
    /// A sphere.
    Sphere {
        /// Radius in metres.
        radius: f32,
    },
    /// A capsule along local Y.
    Capsule {
        /// Radius in metres.
        radius: f32,
        /// Half the segment length between the cap centers, in metres.
        half_height: f32,
    },
    /// An oriented box.
    Box {
        /// Half the size along each local axis, in metres.
        half_extents: Vec3,
    },
}

impl HitShapeKind {
    /// Whether every size is finite and positive (`half_height` may be 0).
    pub fn is_valid(&self) -> bool {
        let ok = |v: f32| v.is_finite() && v > 0.0;
        match *self {
            Self::Sphere { radius } => ok(radius),
            Self::Capsule { radius, half_height } => {
                ok(radius) && half_height.is_finite() && half_height >= 0.0
            }
            Self::Box { half_extents } => ok(half_extents.x) && ok(half_extents.y) && ok(half_extents.z),
        }
    }

    /// The radius of the sphere around the shape's center that holds it at any rotation.
    pub fn bounding_radius(&self) -> f32 {
        match *self {
            Self::Sphere { radius } => radius,
            Self::Capsule { radius, half_height } => radius + half_height,
            Self::Box { half_extents } => half_extents.length(),
        }
    }

    /// The protocol name (`sphere`, `capsule`, `box`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sphere { .. } => "sphere",
            Self::Capsule { .. } => "capsule",
            Self::Box { .. } => "box",
        }
    }
}

/// What a hit shape is attached to.
#[derive(Reflect, Clone, Debug, PartialEq, Default)]
pub enum HitAttach {
    /// The entity's own transform.
    #[default]
    Root,
    /// A skeleton joint, by name (used with [`PoseSource::Joints`]; with [`PoseSource::Root`] the
    /// shape follows the root).
    Joint(Name),
}

/// What joint-attached shapes follow.
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoseSource {
    /// Shapes follow the entity's transform only. Cheapest; joint-attached shapes sit at their
    /// offset from the root.
    #[default]
    Root,
    /// Shapes follow joint poses (from [`LagCompJointSource`] or the scene hierarchy).
    Joints,
}

/// Server: this entity's hit shapes are recorded. Inserted by [`lagcomp_record`] on every
/// replicated entity with [`HitShapes`] unless [`NoLagCompensation`] is present.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct LagCompensated;

/// Opt-out: an entity with [`HitShapes`] that gets no history (hit only at the present).
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NoLagCompensation;

/// Server: on a projectile, what spec 75 § 7 needs to judge its impacts at the shooter's view.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct LagCompProjectile {
    /// The tick the projectile was fired.
    pub fire_tick: NetTick,
    /// `fire_tick - NetViewTick(shooter)` at fire time, in ticks ([`LagCompensation::projectile_view_lag`]).
    pub shooter_view_lag: f64,
}

// ---- hooks --------------------------------------------------------------------------------------

/// Static-world ray test: the distance to the first solid hit within `max_dist`, ignoring the
/// colliders of the listed entities (every lag-compensated entity and the shooter's own; sorted,
/// without duplicates).
pub type OcclusionFn = fn(&World, &Ray, f32, &[Entity]) -> Option<f32>;

/// Host hook for occlusion (spec 75 § 5 step 4). The host installs a function that calls its
/// physics scene queries; with none installed nothing occludes.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LagCompOcclusion(pub Option<OcclusionFn>);

/// The world transform of the joint named `joint` under the root entity, at the tick being recorded.
pub type JointPoseFn = fn(&World, Entity, &str) -> Option<Affine3A>;

/// Host hook for [`PoseSource::Joints`]. With none installed, or when it returns `None`, the
/// recorder looks the joint up in the root's scene hierarchy (spec 90 § 4 (c)).
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LagCompJointSource(pub Option<JointPoseFn>);

// ---- errors and results -------------------------------------------------------------------------

/// Why a lag-compensated query could not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LagCompError {
    /// Queries run on the server (or offline): `net_not_server`.
    #[error("lag-compensated queries run on the server")]
    NotServer,
    /// No [`LagCompHistory`] on this world: `lagcomp_not_installed`.
    #[error("lag compensation is not installed")]
    NotInstalled,
    /// The shooter's peer is not connected: `peer_unknown`.
    #[error("the shooter's peer is not connected")]
    PeerUnknown,
    /// An explicit tick outside the recorded range: `tick_out_of_history`.
    #[error("the tick is outside the recorded history")]
    TickOutOfHistory,
}

impl LagCompError {
    /// The protocol error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotServer => "net_not_server",
            Self::NotInstalled => "lagcomp_not_installed",
            Self::PeerUnknown => "peer_unknown",
            Self::TickOutOfHistory => "tick_out_of_history",
        }
    }
}

/// A lag-compensated hit.
#[derive(Debug, Clone, PartialEq)]
pub struct LagHit {
    /// The entity hit (it may no longer exist: see `alive`).
    pub entity: Entity,
    /// Its id.
    pub net_id: NetId,
    /// The tag of the shape hit.
    pub tag: Name,
    /// The contact point on the shape.
    pub point: Vec3,
    /// The outward surface normal there.
    pub normal: Vec3,
    /// Distance along the query ray (0 for overlaps: the gap from the center).
    pub distance: f32,
    /// The tick the shapes were posed at.
    pub rewound_tick: NetTickFrac,
    /// `false` when the entity was despawned (or lost its `HitShapes`) since.
    pub alive: bool,
}

/// Which candidates a query may hit.
#[derive(Debug, Clone, Default)]
pub struct HitFilter {
    /// Never hit these.
    pub exclude: Vec<Entity>,
    /// When set, only entities carrying at least one of these groups.
    pub groups: Option<NetGroups>,
    /// When set, only entities for which it returns `true`.
    pub predicate: Option<fn(&World, Entity) -> bool>,
}

/// The outcome of [`LagCompensation::validate_claim`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimVerdict {
    /// The claim holds: apply its effect.
    Valid(LagHit),
    /// The claim is refused.
    Invalid(ClaimInvalid),
}

/// Why a claim was refused (spec 75 § 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ClaimInvalid {
    /// The view tick is outside the rewind window or too far from the reported view tick.
    ViewTickOutOfRange,
    /// The ray starts too far from the server's muzzle point.
    OriginTooFar,
    /// The ray misses the claimed shape (with the tolerance) at the rewound tick.
    MissedShape,
    /// Static geometry blocks the ray before the shape.
    Occluded,
    /// The target has no history at the claimed tick.
    TargetUnknown,
}

impl ClaimInvalid {
    /// Every reason, in counter order.
    pub const ALL: [ClaimInvalid; 5] = [
        Self::ViewTickOutOfRange,
        Self::OriginTooFar,
        Self::MissedShape,
        Self::Occluded,
        Self::TargetUnknown,
    ];

    /// The protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::ViewTickOutOfRange => "view_tick_out_of_range",
            Self::OriginTooFar => "origin_too_far",
            Self::MissedShape => "missed_shape",
            Self::Occluded => "occluded",
            Self::TargetUnknown => "target_unknown",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// What the server does with [`HitClaim`]s. A claim is a client's word, so with no policy installed
/// every claim is rejected before any handler sees it (ADR-1892).
#[derive(Resource, Clone, Copy, Debug)]
pub enum HitClaimPolicy {
    /// The engine validates each claim before delivery, with the muzzle point this function gives
    /// for the shooter; `None` rejects the claim. Handlers only see claims that passed.
    Validate(fn(&World, NetPlayerId) -> Option<Vec3>),
    /// Claims reach handlers unchecked; the handler must call
    /// [`LagCompensation::validate_claim`] on every one.
    HandlerValidates,
}

/// A client's report that its shot hit `target` (spec 75 § 8): a to-server, unreliable,
/// rate-limited net message. Register it on every process with
/// `app.add_net_message::<HitClaim>()` when the game uses claims, and install a
/// [`HitClaimPolicy`] on the server: without one every claim is rejected.
#[derive(Clone, Debug, PartialEq, crate::Replicate)]
pub struct HitClaim {
    /// The entity the client says it hit.
    pub target: Entity,
    /// The tag of the shape it says it hit.
    pub tag: String,
    /// The shot ray's origin.
    pub origin: Vec3,
    /// The shot ray's direction (normalized by the server).
    pub direction: Vec3,
    /// The fractional server tick the client was showing.
    pub view_tick: f64,
    /// The game's shot counter (for de-duplication by the game).
    pub weapon_seq: u32,
}

impl HitClaim {
    /// The claimed ray.
    pub fn ray(&self) -> Ray {
        Ray::new(self.origin, self.direction)
    }
}

impl NetMessage for HitClaim {
    const DIRECTION: Direction = Direction::ToServer;
    const CHANNEL: crate::message::Channel = crate::message::Channel::Unreliable;
    // Claims are per hit: 8 pellets at 10 shots a second is 80 a second, and bursts stack up.
    const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
        count: 240,
        per_ticks: 60,
    });

    fn validate(&self, cx: &ValidateContext) -> Verdict {
        let muzzle = match cx.world.get_resource::<HitClaimPolicy>() {
            None => return Verdict::Reject("hit_claim_no_policy"),
            Some(HitClaimPolicy::HandlerValidates) => return Verdict::Accept,
            Some(HitClaimPolicy::Validate(muzzle)) => muzzle,
        };
        let shooter = NetPlayerId {
            peer: cx.from,
            local: 0,
        };
        let Some(muzzle) = muzzle(cx.world, shooter) else {
            return Verdict::Reject("hit_claim_no_muzzle");
        };
        match LagCompensation::new(cx.world).and_then(|lc| lc.validate_claim(shooter, self, muzzle)) {
            Ok(ClaimVerdict::Valid(_)) => Verdict::Accept,
            Ok(ClaimVerdict::Invalid(_)) => Verdict::Reject("hit_claim_invalid"),
            Err(_) => Verdict::Reject("hit_claim_unverifiable"),
        }
    }
}
impl ToServerMessage for HitClaim {}

// ---- history ------------------------------------------------------------------------------------

/// One entity on one recorded tick.
#[derive(Debug, Clone)]
struct Entry {
    net_id: NetId,
    entity: Entity,
    /// The resolved owner when it was a peer (`NetOwner` up the chain).
    owner: Option<PeerId>,
    /// Its `NetGroups`: recorded, so a target that has since died still passes a team filter.
    groups: Option<Arc<Vec<String>>>,
    /// The entity teleported on this tick: never interpolate into it.
    cut: bool,
    aabb: Aabb,
    /// How far to grow the boxes when blending into this tick: the largest bounding radius when a
    /// shape turned past `SWING_DOT` since the last record, else 0 (spec 90 § 4 (j)).
    swing: f32,
    start: u32,
    len: u32,
    tags: Arc<[Name]>,
    /// Exact-tick joint poses (ADR-2551): the animation state instead of posed shapes; `len` is
    /// 0 and the shapes are posed from it when a query needs them.
    anim: Option<Box<AnimEntry>>,
}

impl Entry {
    /// How many shapes the entity has on this tick.
    fn count(&self) -> usize {
        self.anim.as_ref().map_or(self.len as usize, |a| a.shapes.len())
    }
}

/// An entity recorded as animation state (ADR-2551).
#[derive(Debug, Clone)]
struct AnimEntry {
    /// The host's record of the tick's animation state.
    state: Arc<dyn RecordedAnim>,
    /// The root's world rotation, translation and uniform scale on the tick.
    rotation: Quat,
    translation: Vec3,
    scale: f32,
    /// The shape definitions (shared between ticks while unchanged); a joint the rig lacks was
    /// rewritten to the root when recorded.
    shapes: Arc<[HitShape]>,
}

impl AnimEntry {
    fn root(&self) -> Affine3A {
        Affine3A::from_scale_rotation_translation(Vec3::splat(self.scale), self.rotation, self.translation)
    }

    /// Bytes this tick holds for the entity, counted like spec 75 § 14 counts posed shapes: what
    /// the entry stores inline for the state plus the record's own heap.
    fn bytes(&self) -> usize {
        std::mem::size_of::<Arc<dyn RecordedAnim>>()
            + std::mem::size_of::<Quat>()
            + std::mem::size_of::<Vec3>()
            + std::mem::size_of::<f32>()
            + std::mem::size_of::<Arc<[HitShape]>>()
            + self.state.heap_bytes()
    }

    /// The shapes posed from the recorded state, in definition order. `false` when the host's
    /// evaluation failed (the shapes then sit on the root).
    fn pose(&self, out: &mut Vec<PosedShape>) -> bool {
        let root = self.root();
        let names: Vec<&str> = self
            .shapes
            .iter()
            .filter_map(|s| match &s.attach {
                HitAttach::Joint(n) => Some(n.as_str()),
                HitAttach::Root => None,
            })
            .collect();
        let mut joints = Vec::with_capacity(names.len());
        let ok = names.is_empty() || self.state.joint_poses(&root, &names, &mut joints);
        let mut k = 0usize;
        out.clear();
        for shape in self.shapes.iter() {
            let (rot, pos) = match &shape.attach {
                HitAttach::Joint(_) => {
                    let j = joints.get(k).copied().flatten().filter(|_| ok);
                    k += 1;
                    j.map_or((self.rotation, self.translation), |a| rot_trans(&a))
                }
                HitAttach::Root => (self.rotation, self.translation),
            };
            out.push(PosedShape {
                kind: shape.kind,
                position: pos + rot * shape.offset.translation,
                // Scalar-order normalization (spec 71 § 12), see `rot_trans`.
                rotation: crate::scalar::normalize4((rot * shape.offset.rotation).to_array())
                    .map_or(Quat::IDENTITY, Quat::from_array),
            });
        }
        ok
    }
}

/// Every lag-compensated entity on one tick, sorted by `NetId`.
#[derive(Debug, Default, Clone)]
struct TickRecord {
    tick: u32,
    entries: Vec<Entry>,
    shapes: Vec<PosedShape>,
    /// Entry indices per 8 m cell. Built once, when the record is posed.
    grid: HashMap<(i32, i32, i32), Vec<u32>>,
    /// Entries too large, or non-finite, to sit in `grid`. Every query visits them.
    wide: Vec<u32>,
}

impl TickRecord {
    fn clear(&mut self, tick: u32) {
        self.tick = tick;
        self.entries.clear();
        self.shapes.clear();
        self.grid.clear();
        self.wide.clear();
    }

    fn cell_index(v: f32) -> Option<i32> {
        v.is_finite().then(|| (v / LAGCOMP_CELL).floor() as i32)
    }

    /// `(x0, y0, z0, x1, y1, z1)` of the cells `aabb` overlaps, when every corner is finite.
    fn cell_box(aabb: &Aabb) -> Option<(i32, i32, i32, i32, i32, i32)> {
        Some((
            Self::cell_index(aabb.min.x)?,
            Self::cell_index(aabb.min.y)?,
            Self::cell_index(aabb.min.z)?,
            Self::cell_index(aabb.max.x)?,
            Self::cell_index(aabb.max.y)?,
            Self::cell_index(aabb.max.z)?,
        ))
    }

    /// Cells one axis spans, or `None` when the box is inverted.
    ///
    /// A box that covers the whole `i32` cell range saturates instead of overflowing, and the
    /// caller then treats it as too large for the grid.
    fn cell_span(x0: i32, y0: i32, z0: i32, x1: i32, y1: i32, z1: i32) -> Option<i32> {
        if x1 < x0 || y1 < y0 || z1 < z0 {
            return None;
        }
        Some(
            x1.saturating_sub(x0)
                .max(y1.saturating_sub(y0))
                .max(z1.saturating_sub(z0)),
        )
    }

    /// Fills `grid` and `wide` from the entries' boxes. Call after the poses are final.
    fn build_grid(&mut self) {
        self.grid.clear();
        self.wide.clear();
        for (i, e) in self.entries.iter().enumerate() {
            let idx = i as u32;
            let Some((x0, y0, z0, x1, y1, z1)) = Self::cell_box(&e.aabb) else {
                self.wide.push(idx);
                continue;
            };
            let Some(span) = Self::cell_span(x0, y0, z0, x1, y1, z1) else {
                self.wide.push(idx);
                continue;
            };
            if span > CELL_SPAN_MAX {
                self.wide.push(idx);
                continue;
            }
            for x in x0..=x1 {
                for y in y0..=y1 {
                    for z in z0..=z1 {
                        self.grid.entry((x, y, z)).or_default().push(idx);
                    }
                }
            }
        }
    }

    /// Moves boxes and shapes by `delta` and rebuilds the broad phase.
    fn translate(&mut self, delta: Vec3) {
        for entry in &mut self.entries {
            entry.aabb.min += delta;
            entry.aabb.max += delta;
            if let Some(anim) = &mut entry.anim {
                anim.translation += delta;
            }
        }
        for shape in &mut self.shapes {
            shape.position += delta;
        }
        self.build_grid();
    }

    /// Entry indices whose boxes can meet `volume` grown by `grow`.
    ///
    /// `true` means the caller must visit every entry: the query is too large, non-finite, or
    /// this record was never gridded.
    fn indices_for(&self, volume: &Aabb, grow: f32, out: &mut Vec<u32>) -> bool {
        if self.grid.is_empty() && self.wide.is_empty() {
            return !self.entries.is_empty();
        }
        let query = geom::grow(volume, grow.max(0.0));
        let Some((x0, y0, z0, x1, y1, z1)) = Self::cell_box(&query) else {
            return true;
        };
        let Some(span) = Self::cell_span(x0, y0, z0, x1, y1, z1) else {
            return true;
        };
        if span > CELL_SPAN_MAX {
            return true;
        }
        out.clear();
        for x in x0..=x1 {
            for y in y0..=y1 {
                for z in z0..=z1 {
                    if let Some(cell) = self.grid.get(&(x, y, z)) {
                        out.extend_from_slice(cell);
                    }
                }
            }
        }
        out.extend_from_slice(&self.wide);
        out.sort_unstable();
        out.dedup();
        false
    }

    fn find(&self, id: NetId) -> Option<&Entry> {
        self.entries
            .binary_search_by_key(&id, |e| e.net_id)
            .ok()
            .map(|i| &self.entries[i])
    }

    fn shapes_of(&self, e: &Entry) -> &[PosedShape] {
        let a = e.start as usize;
        &self.shapes[a..a + e.len as usize]
    }

    /// The entry's shapes on this tick: recorded, or posed from its animation state.
    fn posed(&self, e: &Entry, counters: &Counters) -> Vec<PosedShape> {
        let Some(anim) = &e.anim else {
            return self.shapes_of(e).to_vec();
        };
        let mut out = Vec::new();
        counters.anim_poses.fetch_add(1, Ordering::Relaxed);
        if !anim.pose(&mut out) {
            counters.anim_failed.fetch_add(1, Ordering::Relaxed);
        }
        // The box was grown by the rig's reach, not taken from a pose: a shape outside it could
        // be missed by the broad phase. Count it and say so once.
        let outside = out.iter().any(|s| {
            let b = s.aabb();
            b.min.cmplt(e.aabb.min - Vec3::splat(1e-3)).any()
                || b.max.cmpgt(e.aabb.max + Vec3::splat(1e-3)).any()
        });
        if outside {
            counters.reach_exceeded.fetch_add(1, Ordering::Relaxed);
            if !counters.reach_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    target: "regia_net",
                    code = "net.lagcomp_reach_exceeded",
                    entity = e.entity.to_bits(),
                    "a hit shape posed from the recorded animation lies outside the box its rig's reach gave"
                );
            }
        }
        out
    }
}

/// Counters for one shooter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ShooterCounters {
    /// Rewound queries made for this shooter.
    pub queries: u64,
    /// Queries whose view tick was clamped.
    pub clamped: u64,
    /// Claims found valid.
    pub claims_valid: u64,
    /// Claims refused, by [`ClaimInvalid`] (counter order of [`ClaimInvalid::ALL`]).
    pub claims_invalid: [u64; 5],
}

impl ShooterCounters {
    /// Refused claims for one reason.
    pub fn invalid(&self, reason: ClaimInvalid) -> u64 {
        self.claims_invalid[reason.index()]
    }
}

/// How much work lag-compensation queries have done since the history was created.
///
/// One query builds one occlusion ignore list, poses each entity's shapes once per view, and
/// considers only the recorded boxes its volume can touch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LagCompPerf {
    /// Occlusion ignore lists built. One query builds one list and reuses it.
    pub ignore_lists: u64,
    /// Fresh shape buffers. A second query of the same view shares the first buffer.
    pub shape_buffers: u64,
    /// Recorded entities whose boxes a query considered.
    pub broadphase_visits: u64,
}

#[derive(Debug, Default)]
struct Counters {
    queries: AtomicU64,
    clamped: AtomicU64,
    /// Sum of rewinds, in 1/256 tick.
    rewind_256: AtomicU64,
    claims_valid: AtomicU64,
    claims_invalid: [AtomicU64; 5],
    /// Entries posed from a recorded animation state (ADR-2551).
    anim_poses: AtomicU64,
    /// Of those, evaluations that failed (shapes on the root).
    anim_failed: AtomicU64,
    /// Posed shapes found outside their recorded box.
    reach_exceeded: AtomicU64,
    reach_warned: AtomicBool,
    /// Occlusion ignore lists built.
    ignore_lists: AtomicU64,
    /// Shape buffers materialized for a query.
    shape_buffers: AtomicU64,
    /// Entities whose boxes a broad phase considered.
    broadphase_visits: AtomicU64,
}

/// One shape in a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShapeReport {
    /// The shape's tag.
    pub tag: String,
    /// `sphere`, `capsule` or `box`.
    pub kind: &'static str,
    /// World-space center.
    pub position: [f32; 3],
    /// World-space rotation (x, y, z, w).
    pub rotation: [f32; 4],
    /// Sizes: `[radius]`, `[radius, half_height]` or `[hx, hy, hz]`.
    pub size: Vec<f32>,
}

impl ShapeReport {
    /// The report of a posed shape.
    pub fn of(shape: &PosedShape, tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            kind: shape.kind.name(),
            position: shape.position.to_array(),
            rotation: shape.rotation.to_array(),
            size: match shape.kind {
                HitShapeKind::Sphere { radius } => vec![radius],
                HitShapeKind::Capsule { radius, half_height } => vec![radius, half_height],
                HitShapeKind::Box { half_extents } => half_extents.to_array().to_vec(),
            },
        }
    }

    /// The posed shape back (`None` when `kind` and `size` do not fit).
    pub fn posed(&self) -> Option<PosedShape> {
        let kind = match (self.kind, self.size.as_slice()) {
            ("sphere", [radius]) => HitShapeKind::Sphere { radius: *radius },
            ("capsule", [radius, half_height]) => HitShapeKind::Capsule {
                radius: *radius,
                half_height: *half_height,
            },
            ("box", [x, y, z]) => HitShapeKind::Box {
                half_extents: Vec3::new(*x, *y, *z),
            },
            _ => return None,
        };
        Some(PosedShape {
            kind,
            position: Vec3::from_array(self.position),
            rotation: Quat::from_array(self.rotation),
        })
    }
}

/// A hit in a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HitReport {
    /// Entity bits.
    pub entity: u64,
    /// Its `NetId`.
    pub net_id: u32,
    /// The shape tag.
    pub tag: String,
    /// Contact point.
    pub point: [f32; 3],
    /// Surface normal.
    pub normal: [f32; 3],
    /// Distance along the ray.
    pub distance: f32,
    /// The tick the shapes were posed at.
    pub rewound_tick: f64,
    /// Whether the entity still exists.
    pub alive: bool,
}

impl HitReport {
    /// The report of a hit.
    pub fn of(h: &LagHit) -> Self {
        Self {
            entity: h.entity.to_bits(),
            net_id: h.net_id.0,
            tag: h.tag.as_str().to_string(),
            point: h.point.to_array(),
            normal: h.normal.to_array(),
            distance: h.distance,
            rewound_tick: h.rewound_tick.0,
            alive: h.alive,
        }
    }
}

/// One logged query (`lagcomp.last_queries`; the rewound shapes of the entity hit are what a debug
/// drawer shows over the shooter's view).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryRecord {
    /// `raycast`, `sphere_cast`, `overlap_sphere` or `claim`.
    pub kind: &'static str,
    /// The shooter (`[peer, local]`), `null` for [`RewoundView`] queries.
    pub shooter: Option<[u32; 2]>,
    /// The server tick the query ran on.
    pub server_tick: u32,
    /// The tick the shapes were posed at.
    pub rewound_tick: f64,
    /// Whether the shooter's view tick was clamped.
    pub clamped: bool,
    /// Ray origin (or overlap center).
    pub origin: [f32; 3],
    /// Ray direction (zero for overlaps).
    pub direction: [f32; 3],
    /// Cast or overlap radius (0 for rays).
    pub radius: f32,
    /// The hit, if any (the nearest for overlaps).
    pub hit: Option<HitReport>,
    /// The hit entity's shapes at the rewound tick.
    pub shapes: Vec<ShapeReport>,
    /// For claims: `valid` or the refusal code.
    pub verdict: Option<&'static str>,
}

/// Server: the hit-shape history (spec 75 § 4), counters and the query log.
#[derive(Resource, Debug, Default)]
pub struct LagCompHistory {
    ticks: VecDeque<TickRecord>,
    pending_cuts: BTreeSet<Entity>,
    warned_joints: HashSet<String>,
    warned_shapes: HashSet<Entity>,
    counters: Counters,
    shooters: Mutex<BTreeMap<NetPlayerId, ShooterCounters>>,
    log: Mutex<VecDeque<QueryRecord>>,
    /// Hits waiting for the debug echo (`lag_compensation.debug_echo`).
    echoes: Mutex<VecDeque<QueryRecord>>,
    /// (tick, queries total, clamped total) per recorded tick over the last second.
    rate_window: VecDeque<(u32, u64, u64)>,
    /// How many times `net.hitshape_joint_missing` was logged.
    pub joint_missing_logged: u32,
    /// `PoseSource::Joints` entities recorded without their animation state, by the capture
    /// hook's reason code (ADR-2551).
    anim_fallbacks: BTreeMap<&'static str, u64>,
    /// The present tick a query already posed, with the world change tick it was posed at.
    /// `record` and later queries adopt it only while that change tick is current: once any
    /// system has run since, the world may have moved and the pose is built again (corr-net,
    /// ADR-5580).
    staged_live: Mutex<Option<StagedLive>>,
}

/// A present-tick pose a query built, and the world change tick it saw.
#[derive(Debug, Clone)]
struct StagedLive {
    /// `World::read_change_tick` when the pose was built.
    at: Tick,
    /// The pose.
    rec: Arc<TickRecord>,
}

impl LagCompHistory {
    /// Moves every recorded box and shape by `delta` (a floating-origin rebase, spec 78 § 12.2),
    /// so a rewind after the rebase tests the shapes where they now are.
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        for record in &mut self.ticks {
            record.translate(delta);
        }
        let mut staged = self.staged_guard();
        if let Some(live) = staged.as_mut() {
            Arc::make_mut(&mut live.rec).translate(delta);
        }
    }

    fn staged_guard(&self) -> std::sync::MutexGuard<'_, Option<StagedLive>> {
        self.staged_live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The present-tick pose a query stored, when it is for `tick` and no system has run since
    /// it was posed (`now` is the world's current change tick).
    fn take_staged(&mut self, tick: u32, now: Tick) -> Option<TickRecord> {
        let mut staged = self.staged_guard();
        let live = staged.take()?;
        if live.rec.tick != tick || live.at != now {
            return None;
        }
        Some(Arc::try_unwrap(live.rec).unwrap_or_else(|a| (*a).clone()))
    }

    /// Ticks currently held.
    pub fn ticks_kept(&self) -> usize {
        self.ticks.len()
    }

    /// The oldest recorded tick.
    pub fn oldest_tick(&self) -> Option<NetTick> {
        self.ticks.front().map(|r| NetTick(r.tick))
    }

    /// The newest recorded tick.
    pub fn newest_tick(&self) -> Option<NetTick> {
        self.ticks.back().map(|r| NetTick(r.tick))
    }

    /// Entities on the newest recorded tick.
    pub fn entities(&self) -> usize {
        self.ticks.back().map_or(0, |r| r.entries.len())
    }

    /// Whether `entity` is on any recorded tick.
    pub fn contains(&self, entity: Entity) -> bool {
        self.ticks
            .iter()
            .any(|r| r.entries.iter().any(|e| e.entity == entity))
    }

    /// Marks `entity` as teleported on the tick being simulated: its next record is a cut and no
    /// rewind interpolates across it (`net_teleport` calls this).
    pub fn mark_cut(&mut self, entity: Entity) {
        self.pending_cuts.insert(entity);
    }

    /// Drops every recorded tick (a level change).
    pub fn clear(&mut self) {
        self.ticks.clear();
        self.pending_cuts.clear();
        *self.staged_guard() = None;
    }

    /// The counters of one shooter.
    pub fn shooter_counters(&self, shooter: NetPlayerId) -> ShooterCounters {
        self.shooters
            .lock()
            .ok()
            .and_then(|m| m.get(&shooter).copied())
            .unwrap_or_default()
    }

    /// Every shooter's counters, in `NetPlayerId` order.
    pub fn all_shooter_counters(&self) -> Vec<(NetPlayerId, ShooterCounters)> {
        self.shooters
            .lock()
            .map(|m| m.iter().map(|(k, v)| (*k, *v)).collect())
            .unwrap_or_default()
    }

    /// The last [`QUERY_LOG_LEN`] queries, oldest first.
    pub fn last_queries(&self) -> Vec<QueryRecord> {
        self.log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Totals: (queries, clamped, claims valid, claims invalid per reason).
    pub fn totals(&self) -> (u64, u64, u64, [u64; 5]) {
        let c = &self.counters;
        (
            c.queries.load(Ordering::Relaxed),
            c.clamped.load(Ordering::Relaxed),
            c.claims_valid.load(Ordering::Relaxed),
            std::array::from_fn(|i| c.claims_invalid[i].load(Ordering::Relaxed)),
        )
    }

    /// Mean rewind of all queries so far, in ticks.
    pub fn mean_rewind_ticks(&self) -> f64 {
        let q = self.counters.queries.load(Ordering::Relaxed);
        if q == 0 {
            return 0.0;
        }
        self.counters.rewind_256.load(Ordering::Relaxed) as f64 / 256.0 / q as f64
    }

    /// Queries and clamps over the last second of recorded ticks.
    pub fn per_second(&self) -> (u64, u64) {
        match (self.rate_window.front(), self.rate_window.back()) {
            (Some(a), Some(b)) => (b.1.saturating_sub(a.1), b.2.saturating_sub(a.2)),
            _ => (0, 0),
        }
    }

    /// Bytes the history holds for `entity` over every kept tick, counted as spec 75 § 14 counts
    /// them (`ticks × (shapes × posed shape + box)`): posed shapes and the box, or for an entity
    /// recorded as animation state (ADR-2551) the box plus the state.
    pub fn entity_bytes(&self, entity: Entity) -> usize {
        self.ticks
            .iter()
            .filter_map(|r| r.entries.iter().find(|e| e.entity == entity))
            .map(|e| {
                std::mem::size_of::<Aabb>()
                    + e.len as usize * std::mem::size_of::<PosedShape>()
                    + e.anim.as_ref().map_or(0, |a| a.bytes())
            })
            .sum()
    }

    /// Entities on the newest tick recorded as animation state (ADR-2551).
    pub fn anim_entities(&self) -> usize {
        self.ticks
            .back()
            .map_or(0, |r| r.entries.iter().filter(|e| e.anim.is_some()).count())
    }

    /// `PoseSource::Joints` records that fell back to posed shapes, by the capture hook's code.
    pub fn anim_fallbacks(&self) -> &BTreeMap<&'static str, u64> {
        &self.anim_fallbacks
    }

    /// (entries posed from animation state, of which failed, posed shapes found outside their
    /// recorded box).
    pub fn anim_counters(&self) -> (u64, u64, u64) {
        let c = &self.counters;
        (
            c.anim_poses.load(Ordering::Relaxed),
            c.anim_failed.load(Ordering::Relaxed),
            c.reach_exceeded.load(Ordering::Relaxed),
        )
    }

    /// Work counters for occlusion lists, shape buffers and broad-phase visits.
    pub fn perf(&self) -> LagCompPerf {
        let c = &self.counters;
        LagCompPerf {
            ignore_lists: c.ignore_lists.load(Ordering::Relaxed),
            shape_buffers: c.shape_buffers.load(Ordering::Relaxed),
            broadphase_visits: c.broadphase_visits.load(Ordering::Relaxed),
        }
    }

    /// `entity`'s shapes on the newest tick that recorded it (the gizmo group's grey shapes):
    /// one evaluation for an entity recorded as animation state, not one per kept tick.
    pub fn newest_shapes_of(&self, entity: Entity) -> Option<Vec<ShapeReport>> {
        self.ticks.iter().rev().find_map(|r| {
            r.entries
                .iter()
                .find(|e| e.entity == entity)
                .map(|e| shape_reports(&r.posed(e, &self.counters), &e.tags))
        })
    }

    /// Every recorded tick of `entity`, oldest first: (tick, cut, shapes).
    pub fn history_of(&self, entity: Entity) -> Vec<(NetTick, bool, Vec<ShapeReport>)> {
        let mut out = Vec::new();
        for r in &self.ticks {
            if let Some(e) = r.entries.iter().find(|e| e.entity == entity) {
                out.push((
                    NetTick(r.tick),
                    e.cut,
                    shape_reports(&r.posed(e, &self.counters), &e.tags),
                ));
            }
        }
        out
    }

    fn note_query(&self, shooter: Option<NetPlayerId>, rewind: f64, clamped: bool) {
        let c = &self.counters;
        c.queries.fetch_add(1, Ordering::Relaxed);
        c.rewind_256
            .fetch_add((rewind.max(0.0) * 256.0).round() as u64, Ordering::Relaxed);
        if clamped {
            c.clamped.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(s) = shooter
            && let Ok(mut m) = self.shooters.lock()
        {
            let e = m.entry(s).or_default();
            e.queries += 1;
            e.clamped += u64::from(clamped);
        }
    }

    fn note_claim(&self, shooter: NetPlayerId, verdict: Result<(), ClaimInvalid>) {
        let c = &self.counters;
        match verdict {
            Ok(()) => c.claims_valid.fetch_add(1, Ordering::Relaxed),
            Err(r) => c.claims_invalid[r.index()].fetch_add(1, Ordering::Relaxed),
        };
        if let Ok(mut m) = self.shooters.lock() {
            let e = m.entry(shooter).or_default();
            match verdict {
                Ok(()) => e.claims_valid += 1,
                Err(r) => e.claims_invalid[r.index()] += 1,
            }
        }
    }

    fn log_query(&self, record: QueryRecord) {
        if let Ok(mut l) = self.log.lock() {
            if l.len() >= QUERY_LOG_LEN {
                l.pop_front();
            }
            l.push_back(record);
        }
    }

    fn queue_echo(&self, record: &QueryRecord) {
        let remote = record.shooter.is_some_and(|s| s[0] != PeerId::SERVER.0);
        if !remote || record.hit.is_none() {
            return;
        }
        if let Ok(mut q) = self.echoes.lock() {
            if q.len() >= ECHO_QUEUE_LEN {
                q.pop_front();
            }
            q.push_back(record.clone());
        }
    }

    /// Takes the hits waiting for the debug echo, oldest first.
    pub fn take_echoes(&self) -> Vec<QueryRecord> {
        self.echoes
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Records tick `tick` from the world (what [`lagcomp_record`] does after its checks).
    fn record(
        &mut self,
        world: &World,
        tick: u32,
        settings: &LagCompSettings,
        capacity: usize,
        hz: f64,
        owners: &mut OwnerLookup<'_>,
    ) {
        // Re-recording the same tick (a second call) replaces it.
        while self.ticks.back().is_some_and(|r| r.tick >= tick) {
            self.ticks.pop_back();
        }
        while self.ticks.len() >= capacity.max(1) {
            self.ticks.pop_front();
        }
        let cuts = std::mem::take(&mut self.pending_cuts);
        // A query of this tick already posed the world. Reuse that record: the shapes match
        // what the shot tested, and the joints are not walked again.
        let mut rec = if let Some(rec) = self.take_staged(tick, world.read_change_tick()) {
            rec
        } else {
            let mut rec = TickRecord::default();
            rec.clear(tick);
            let source = world.get_resource::<LagCompJointSource>().and_then(|s| s.0);
            let mut warn = Warnings {
                joints: &mut self.warned_joints,
                shapes: &mut self.warned_shapes,
                joint_missing_logged: &mut self.joint_missing_logged,
                anim_fallbacks: &mut self.anim_fallbacks,
            };
            pose_world(
                world,
                &mut rec,
                settings,
                Sources {
                    joints: source,
                    anim: world.get_resource::<LagCompAnimSource>().and_then(|s| s.0),
                    tick_secs: (1.0 / hz) as f32,
                },
                Some(&mut warn),
                self.ticks.back(),
                owners,
            );
            rec
        };
        for e in &mut rec.entries {
            e.cut = cuts.contains(&e.entity);
        }
        self.ticks.push_back(rec);
        let c = &self.counters;
        self.rate_window.push_back((
            tick,
            c.queries.load(Ordering::Relaxed),
            c.clamped.load(Ordering::Relaxed),
        ));
        let keep = hz.round().max(1.0) as usize + 1;
        while self.rate_window.len() > keep {
            self.rate_window.pop_front();
        }
    }
}

struct Warnings<'a> {
    joints: &'a mut HashSet<String>,
    shapes: &'a mut HashSet<Entity>,
    joint_missing_logged: &'a mut u32,
    anim_fallbacks: &'a mut BTreeMap<&'static str, u64>,
}

/// Where joint poses come from while recording.
#[derive(Clone, Copy)]
struct Sources {
    joints: Option<JointPoseFn>,
    anim: Option<AnimCaptureFn>,
    /// One fixed step, in seconds.
    tick_secs: f32,
}

impl Warnings<'_> {
    fn joint_missing(&mut self, name: &str) {
        if self.joints.insert(name.to_string()) {
            *self.joint_missing_logged += 1;
            tracing::warn!(
                target: "regia_net",
                code = "net.hitshape_joint_missing",
                joint = name,
                "hit shape joint not found; the shape follows the root"
            );
        }
    }
}

fn shape_reports(shapes: &[PosedShape], tags: &[Name]) -> Vec<ShapeReport> {
    shapes
        .iter()
        .zip(tags.iter())
        .map(|(s, t)| ShapeReport::of(s, t.as_str()))
        .collect()
}

// ---- posing -------------------------------------------------------------------------------------

fn local_pose(ent: &EntityRef) -> Option<Affine3A> {
    ent.get::<Transform>()
        .map(|t| Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation))
}

/// The fresh world pose of an entity: its `Transform` composed with its ancestors'. Recording runs
/// inside the fixed step, where a `GlobalTransform` can be a frame old. An ancestor without a
/// `Transform` ends the walk at its `GlobalTransform`.
pub fn root_pose(world: &World, ent: &EntityRef) -> Affine3A {
    let Some(mut out) = local_pose(ent) else {
        return ent.get::<GlobalTransform>().map_or(Affine3A::IDENTITY, |g| g.0);
    };
    let mut parent = ent.get::<ChildOf>().map(ChildOf::parent);
    for _ in 0..PARENT_WALK_LIMIT {
        let Some(e) = parent.and_then(|p| world.get_entity(p).ok()) else {
            break;
        };
        let Some(local) = local_pose(&e) else {
            return e.get::<GlobalTransform>().map_or(out, |g| g.0 * out);
        };
        out = local * out;
        parent = e.get::<ChildOf>().map(ChildOf::parent);
    }
    out
}

fn rot_trans(a: &Affine3A) -> (Quat, Vec3) {
    let (_, r, t) = a.to_scale_rotation_translation();
    // Scalar-order normalization (spec 71 § 12): the decompose above is left-to-right on every
    // platform (its dot3-based column lengths agree on SSE2 and NEON), but glam's `Quat::normalize`
    // is a SIMD dot4 that pairs components differently per architecture; the recorded pose would
    // differ between an x86_64 listen server and an aarch64 server for the same transforms.
    let r = crate::scalar::normalize4(r.to_array()).map_or(Quat::IDENTITY, Quat::from_array);
    (r, t)
}

/// The joint's world pose from the scene hierarchy: the descendant whose scene `Name` is `joint`,
/// its `GlobalTransform` relative to the root's, composed with the root's fresh pose.
fn hierarchy_joint(world: &World, root: Entity, root_now: &Affine3A, joint: &str) -> Option<Affine3A> {
    let root_ref = world.get_entity(root).ok()?;
    let root_g = root_ref.get::<GlobalTransform>()?.0;
    let mut queue: VecDeque<Entity> = root_ref
        .get::<Children>()
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let mut seen = 0usize;
    while let Some(e) = queue.pop_front() {
        seen += 1;
        if seen > JOINT_SEARCH_LIMIT {
            return None;
        }
        let Ok(ent) = world.get_entity(e) else {
            continue;
        };
        if ent.get::<Name>().is_some_and(|n| n.as_str() == joint) {
            let g = ent.get::<GlobalTransform>()?.0;
            return Some(*root_now * root_g.inverse() * g);
        }
        if let Some(c) = ent.get::<Children>() {
            queue.extend(c.iter());
        }
    }
    None
}

/// One record's owner cache. The same signature reuses resolutions instead of walking again.
struct OwnerLookup<'a> {
    cache: &'a mut OwnerCache,
    sig: (u64, u64),
}

impl OwnerLookup<'_> {
    /// Peer owner bit stored on a history entry: an owned peer, or `None` for server and broken.
    fn owner_bit(&mut self, world: &World, entity: Entity) -> Option<PeerId> {
        match self.cache.record_owner(world, entity, self.sig) {
            OwnerResolution::Owned(p) => Some(p),
            OwnerResolution::Server | OwnerResolution::Broken => None,
        }
    }
}

/// Poses every lag-compensated entity into `out` (sorted by `NetId`). `prev` is the last record:
/// unchanged tag lists are shared with it.
fn pose_world(
    world: &World,
    out: &mut TickRecord,
    settings: &LagCompSettings,
    sources: Sources,
    mut warn: Option<&mut Warnings<'_>>,
    prev: Option<&TickRecord>,
    owners: &mut OwnerLookup<'_>,
) {
    let joint_source = sources.joints;
    let Some(mut q) =
        world.try_query_filtered::<(Entity, &NetId, &HitShapes), (With<LagCompensated>, Without<NoLagCompensation>)>()
    else {
        out.build_grid();
        return;
    };
    let mut items: Vec<(NetId, Entity)> = q.iter(world).map(|(e, id, _)| (*id, e)).collect();
    items.sort_unstable();
    let max_shapes = settings.max_shapes_per_entity as usize;
    let mut tags: Vec<Name> = Vec::new();
    for (net_id, entity) in items {
        let Ok(ent) = world.get_entity(entity) else {
            continue;
        };
        let Some(hs) = ent.get::<HitShapes>() else {
            continue;
        };
        let root = root_pose(world, &ent);
        let (root_rot, root_pos) = rot_trans(&root);
        let last = prev.and_then(|p| p.find(net_id).map(|e| (p, e)));
        if hs.pose_source == PoseSource::Joints
            && let Some(capture) = sources.anim
        {
            let previous = last.and_then(|(_, e)| e.anim.as_ref()).map(|a| &a.state);
            match capture(world, entity, sources.tick_secs, previous) {
                Ok(state) => {
                    let inputs = AnimInputs {
                        entity,
                        net_id,
                        hs,
                        root,
                        state,
                    };
                    if let Some(entry) =
                        anim_entry(world, inputs, settings, warn.as_deref_mut(), last, owners)
                    {
                        out.entries.push(entry);
                    }
                    continue;
                }
                Err(code) => {
                    if let Some(w) = warn.as_deref_mut() {
                        *w.anim_fallbacks.entry(code).or_default() += 1;
                    }
                }
            }
        }
        let start = out.shapes.len();
        tags.clear();
        let mut bad = hs.shapes.len() > max_shapes;
        let mut aabb: Option<Aabb> = None;
        // One resolve per joint name. Several shapes on the same bone share it.
        // The name is owned: it has to outlive the shape the loop is visiting.
        let mut joint_cache: Vec<(String, Option<(Quat, Vec3)>)> = Vec::new();
        for shape in hs.shapes.iter().take(max_shapes) {
            if !shape.kind.is_valid() {
                bad = true;
                continue;
            }
            let (rot, pos) = match (&shape.attach, hs.pose_source) {
                (HitAttach::Joint(name), PoseSource::Joints) => {
                    let name = name.as_str();
                    let pose = if let Some((_, pose)) = joint_cache.iter().find(|(n, _)| n == name) {
                        *pose
                    } else {
                        let pose = joint_source
                            .and_then(|f| f(world, entity, name))
                            .or_else(|| hierarchy_joint(world, entity, &root, name))
                            .map(|a| rot_trans(&a));
                        if pose.is_none()
                            && let Some(w) = warn.as_deref_mut()
                        {
                            w.joint_missing(name);
                        }
                        joint_cache.push((name.to_string(), pose));
                        pose
                    };
                    pose.unwrap_or((root_rot, root_pos))
                }
                _ => (root_rot, root_pos),
            };
            let q = rot * shape.offset.rotation;
            let posed = PosedShape {
                kind: shape.kind,
                position: pos + rot * shape.offset.translation,
                // Scalar-order normalization (spec 71 § 12), see `rot_trans`.
                rotation: crate::scalar::normalize4(q.to_array()).map_or(Quat::IDENTITY, Quat::from_array),
            };
            let b = posed.aabb();
            aabb = Some(aabb.map_or(b, |a| geom::union(&a, &b)));
            out.shapes.push(posed);
            tags.push(shape.tag.clone());
        }
        if bad
            && let Some(w) = warn.as_deref_mut()
            && w.shapes.insert(entity)
        {
            tracing::warn!(
                target: "regia_net",
                code = "net.hitshape_invalid",
                entity = entity.to_bits(),
                "hit shapes with a non-positive or non-finite size, or more than max_shapes_per_entity, are ignored"
            );
        }
        let Some(aabb) = aabb else {
            continue;
        };
        let shared = last
            .filter(|(_, e)| *e.tags == *tags.as_slice())
            .map(|(_, e)| e.tags.clone());
        let groups = ent.get::<NetGroups>().map(|g| {
            last.and_then(|(_, e)| e.groups.clone())
                .filter(|kept| **kept == g.0)
                .unwrap_or_else(|| Arc::new(g.0.clone()))
        });
        let posed = &out.shapes[start..];
        let swing = last.map_or(0.0, |(p, e)| {
            // Scalar-order dot (spec 71 § 12): a swing near `SWING_DOT` rounds differently
            // under glam's SSE2/NEON dot4 pairings, and one server would grow the broadphase
            // bounds while the other does not.
            let turned = p
                .shapes_of(e)
                .iter()
                .zip(posed)
                .any(|(a, b)| crate::scalar::quat_dot(a.rotation, b.rotation).abs() < SWING_DOT);
            if turned {
                posed.iter().map(|s| s.kind.bounding_radius()).fold(0.0, f32::max)
            } else {
                0.0
            }
        });
        let owner = owners.owner_bit(world, entity);
        out.entries.push(Entry {
            net_id,
            entity,
            owner,
            groups,
            cut: false,
            aabb,
            swing,
            start: start as u32,
            len: (out.shapes.len() - start) as u32,
            tags: shared.unwrap_or_else(|| Arc::from(tags.as_slice())),
            anim: None,
        });
    }
    out.build_grid();
}

/// What `anim_entry` records for one entity.
struct AnimInputs<'a> {
    entity: Entity,
    net_id: NetId,
    hs: &'a HitShapes,
    /// The root's fresh world pose.
    root: Affine3A,
    state: Arc<dyn RecordedAnim>,
}

/// The entry of a `PoseSource::Joints` entity recorded as animation state (ADR-2551): no pose is
/// evaluated here; the box is the root's position grown by the rig's reach and each shape's
/// extent. `None` when no shape is valid.
fn anim_entry(
    world: &World,
    inputs: AnimInputs<'_>,
    settings: &LagCompSettings,
    mut warn: Option<&mut Warnings<'_>>,
    last: Option<(&TickRecord, &Entry)>,
    owners: &mut OwnerLookup<'_>,
) -> Option<Entry> {
    let AnimInputs {
        entity,
        net_id,
        hs,
        root,
        state,
    } = inputs;
    let max_shapes = settings.max_shapes_per_entity as usize;
    let (scale, rotation, translation) = root.to_scale_rotation_translation();
    let scale = scale.abs().max_element();
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut shapes: Vec<HitShape> = Vec::with_capacity(hs.shapes.len().min(max_shapes));
    let mut bad = hs.shapes.len() > max_shapes;
    let mut extent = 0.0f32;
    for shape in hs.shapes.iter().take(max_shapes) {
        if !shape.kind.is_valid() {
            bad = true;
            continue;
        }
        let mut shape = shape.clone();
        if let HitAttach::Joint(name) = &shape.attach
            && !state.has_joint(name.as_str())
        {
            if let Some(w) = warn.as_deref_mut() {
                w.joint_missing(name.as_str());
            }
            shape.attach = HitAttach::Root;
        }
        extent = extent.max(shape.offset.translation.length() + shape.kind.bounding_radius());
        shapes.push(shape);
    }
    if bad
        && let Some(w) = warn
        && w.shapes.insert(entity)
    {
        tracing::warn!(
            target: "regia_net",
            code = "net.hitshape_invalid",
            entity = entity.to_bits(),
            "hit shapes with a non-positive or non-finite size, or more than max_shapes_per_entity, are ignored"
        );
    }
    if shapes.is_empty() {
        return None;
    }
    let reach = state.reach().max(0.0) * scale + extent;
    let reach = if reach.is_finite() { reach } else { 0.0 };
    let aabb = Aabb {
        min: translation - Vec3::splat(reach),
        max: translation + Vec3::splat(reach),
    };
    let prev_anim = last.and_then(|(_, e)| e.anim.as_ref());
    let shapes: Arc<[HitShape]> = match prev_anim {
        Some(a) if *a.shapes == *shapes.as_slice() => a.shapes.clone(),
        _ => Arc::from(shapes),
    };
    let tags: Arc<[Name]> = match last {
        Some((_, e))
            if e.tags.len() == shapes.len() && e.tags.iter().zip(shapes.iter()).all(|(t, s)| *t == s.tag) =>
        {
            e.tags.clone()
        }
        _ => shapes.iter().map(|s| s.tag.clone()).collect(),
    };
    let ent = world.get_entity(entity).ok()?;
    let groups = ent.get::<NetGroups>().map(|g| {
        last.and_then(|(_, e)| e.groups.clone())
            .filter(|kept| **kept == g.0)
            .unwrap_or_else(|| Arc::new(g.0.clone()))
    });
    let owner = owners.owner_bit(world, entity);
    Some(Entry {
        net_id,
        entity,
        owner,
        groups,
        cut: false,
        aabb,
        swing: 0.0,
        start: 0,
        len: 0,
        tags,
        anim: Some(Box::new(AnimEntry {
            state,
            // Scalar-order normalization (spec 71 § 12), see `rot_trans`.
            rotation: crate::scalar::normalize4(rotation.to_array()).map_or(Quat::IDENTITY, Quat::from_array),
            translation,
            scale,
            shapes,
        })),
    })
}

// ---- recording ----------------------------------------------------------------------------------

/// The fixed-step rate (`FixedTime::hz`, 60 when absent).
fn tick_hz(world: &World) -> f64 {
    world
        .get_resource::<FixedTime>()
        .map(|t| t.hz)
        .filter(|h| h.is_finite() && *h > 0.0)
        .unwrap_or(60.0)
}

/// `ceil(max_rewind_ms / step_ms)`, with `max_rewind_ms` at most [`MAX_REWIND_CEILING_MS`]: the
/// project maximum of every shooter's window (spec 120 § 9.6) and the depth of the history.
pub fn max_rewind_ticks(settings: &LagCompSettings, hz: f64) -> u32 {
    let ms = window::cap_ms(settings.max_rewind_ms);
    // A tiny slack keeps 250 ms at 60 Hz at exactly 15 ticks despite rounding.
    ((ms * hz / 1000.0) - 1e-9).ceil().clamp(0.0, f64::from(u16::MAX)) as u32
}

/// Server, end of each fixed step (`NetSet::LagCompRecord`): marks replicated entities with
/// [`HitShapes`] as [`LagCompensated`] and records the tick's shape poses. Does nothing on
/// clients or without a [`LagCompHistory`].
pub fn lagcomp_record(world: &mut World) {
    let _span = tracing::info_span!("net_lagcomp_record").entered();
    if !world.get_resource::<NetMode>().is_some_and(|m| m.is_server())
        || !world.contains_resource::<LagCompHistory>()
    {
        return;
    }
    let mut q = world.query_filtered::<Entity, (
        With<HitShapes>,
        With<NetId>,
        Without<LagCompensated>,
        Without<NoLagCompensation>,
    )>();
    let mark: Vec<Entity> = q.iter(world).collect();
    for e in mark {
        world.entity_mut(e).insert(LagCompensated);
    }
    let tick = world.get_resource::<NetNode>().map_or(0, |n| n.tick.0);
    let settings = world
        .get_resource::<NetSettings>()
        .map(|s| s.lag_compensation.clone())
        .unwrap_or_default();
    let hz = tick_hz(world);
    let capacity = max_rewind_ticks(&settings, hz) as usize + 2;
    // ChildOf is part of the record signature and not of gather's owner signature, so a
    // parent-only edit invalidates this cache without a full owner walk of every replicated entity.
    let sig = crate::replication::owner_record_signature(world);
    let mut cache = world.remove_resource::<OwnerCache>().unwrap_or_default();
    world.resource_scope(|world, mut history: Mut<LagCompHistory>| {
        let mut owners = OwnerLookup {
            cache: &mut cache,
            sig,
        };
        history.record(world, tick, &settings, capacity, hz, &mut owners);
    });
    cache.drop_recorded_gone(world);
    world.insert_resource(cache);
}

/// Inserts the lag-compensation resources on a world (what [`LagCompPlugin`] does, for worlds
/// without an `App`, such as the loopback hub).
pub fn install_lag_compensation(world: &mut World) {
    world.init_resource::<LagCompHistory>();
    world.init_resource::<LagCompOcclusion>();
    world.init_resource::<LagCompJointSource>();
    world.init_resource::<LagCompAnimSource>();
    world.init_resource::<LagCompWindows>();
}

/// Turns lag compensation on: the history resources, the hooks and [`lagcomp_record`] in
/// `NetSet::LagCompRecord`. Add it after `NetPlugin`, on servers (it does nothing on clients).
#[derive(Debug, Clone, Copy, Default)]
pub struct LagCompPlugin;

impl Plugin for LagCompPlugin {
    fn build(&self, app: &mut App) {
        install_lag_compensation(app.world_mut());
        app.register_type::<HitShapes>()
            .register_type::<LagCompensated>()
            .register_type::<NoLagCompensation>();
        app.add_systems(FixedUpdate, lagcomp_record.in_set(NetSet::LagCompRecord));
    }
}

// ---- queries ------------------------------------------------------------------------------------

/// Shapes posed from recorded animation states during one view, by (tick, `NetId`): every query
/// of a tick shares one evaluation per entity and record.
/// Posed shapes by (tick, `NetId`).
type PosedByTick = HashMap<(u32, NetId), Arc<[PosedShape]>>;

#[derive(Default)]
struct PoseCache(RefCell<PosedByTick>);

impl PoseCache {
    /// The entity's shapes for this record. The first query of a view builds the buffer; the
    /// rest share it.
    fn shapes(&self, rec: &TickRecord, e: &Entry, counters: &Counters) -> Arc<[PosedShape]> {
        let key = (rec.tick, e.net_id);
        if let Some(s) = self.0.borrow().get(&key) {
            return Arc::clone(s);
        }
        let posed = if e.anim.is_some() {
            rec.posed(e, counters)
        } else {
            rec.shapes_of(e).to_vec()
        };
        let arc = Arc::<[PosedShape]>::from(posed);
        counters.shape_buffers.fetch_add(1, Ordering::Relaxed);
        self.0.borrow_mut().insert(key, Arc::clone(&arc));
        arc
    }
}

/// Two records around a fractional tick.
struct Bracket<'r> {
    a: &'r TickRecord,
    b: Option<&'r TickRecord>,
    alpha: f32,
    tick: f64,
    cache: &'r PoseCache,
    counters: &'r Counters,
}

/// A candidate posed at the bracket's tick.
struct Posed {
    net_id: NetId,
    entity: Entity,
    owner: Option<PeerId>,
    groups: Option<Arc<Vec<String>>>,
    shapes: Arc<[PosedShape]>,
    tags: Arc<[Name]>,
}

/// The nearest cast: the hit, and the shapes and tags that query already posed.
struct CastMatch {
    hit: LagHit,
    shapes: Arc<[PosedShape]>,
    tags: Arc<[Name]>,
}

/// Which of the two entries a candidate is posed from.
enum Choice<'r> {
    A(&'r Entry),
    B(&'r Entry),
    Blend(&'r Entry, &'r Entry),
}

impl<'r> Bracket<'r> {
    /// The candidates in `NetId` order whose entity and box (grown by `grow`) pass `test`.
    ///
    /// `volume` is the query's box before `grow`. A single record visits the uniform grid.
    /// A blend of two records stays a linear merge: the union of the two boxes covers the
    /// gap between them, which neither record's grid contains.
    fn candidates(&self, grow: f32, volume: Aabb, test: impl Fn(Entity, &Aabb) -> bool) -> Vec<Posed> {
        if self.b.is_none() {
            return self.candidates_grid(grow, volume, test);
        }
        self.candidates_linear(grow, test)
    }

    fn candidates_grid(&self, grow: f32, volume: Aabb, test: impl Fn(Entity, &Aabb) -> bool) -> Vec<Posed> {
        let mut index = Vec::new();
        let all = self.a.indices_for(&volume, grow, &mut index);
        let mut out = Vec::new();
        if all {
            for e in &self.a.entries {
                self.counters.broadphase_visits.fetch_add(1, Ordering::Relaxed);
                if test(e.entity, &geom::grow(&e.aabb, grow)) {
                    out.push(self.pose(Choice::A(e)));
                }
            }
        } else {
            for i in index {
                let Some(e) = self.a.entries.get(i as usize) else {
                    continue;
                };
                self.counters.broadphase_visits.fetch_add(1, Ordering::Relaxed);
                if test(e.entity, &geom::grow(&e.aabb, grow)) {
                    out.push(self.pose(Choice::A(e)));
                }
            }
        }
        out
    }

    fn candidates_linear(&self, grow: f32, test: impl Fn(Entity, &Aabb) -> bool) -> Vec<Posed> {
        let mut out = Vec::new();
        let empty: [Entry; 0] = [];
        let eb: &[Entry] = self.b.map_or(&empty[..], |b| b.entries.as_slice());
        let (mut i, mut j) = (0usize, 0usize);
        let ea = &self.a.entries;
        while i < ea.len() || j < eb.len() {
            let (x, y) = match (ea.get(i), eb.get(j)) {
                (Some(x), Some(y)) if x.net_id == y.net_id => {
                    i += 1;
                    j += 1;
                    (Some(x), Some(y))
                }
                (Some(x), Some(y)) if x.net_id < y.net_id => {
                    i += 1;
                    (Some(x), None)
                }
                (Some(_), Some(y)) => {
                    j += 1;
                    (None, Some(y))
                }
                (Some(x), None) => {
                    i += 1;
                    (Some(x), None)
                }
                (None, Some(y)) => {
                    j += 1;
                    (None, Some(y))
                }
                (None, None) => break,
            };
            let near_a = self.alpha < 0.5;
            let choice = match (x, y) {
                (Some(x), None) if near_a => Choice::A(x),
                (None, Some(y)) if !near_a => Choice::B(y),
                (Some(x), Some(y)) if y.cut || x.count() != y.count() => {
                    if near_a {
                        Choice::A(x)
                    } else {
                        Choice::B(y)
                    }
                }
                (Some(x), Some(y)) => Choice::Blend(x, y),
                _ => continue,
            };
            let (entity, bounds) = match choice {
                Choice::A(x) => (x.entity, x.aabb),
                Choice::B(y) => (y.entity, y.aabb),
                Choice::Blend(x, y) => (x.entity, geom::grow(&geom::union(&x.aabb, &y.aabb), y.swing)),
            };
            self.counters.broadphase_visits.fetch_add(1, Ordering::Relaxed);
            if !test(entity, &geom::grow(&bounds, grow)) {
                continue;
            }
            out.push(self.pose(choice));
        }
        out
    }

    fn pose(&self, choice: Choice<'_>) -> Posed {
        let (e, shapes) = match choice {
            Choice::A(x) => (x, self.cache.shapes(self.a, x, self.counters)),
            Choice::B(y) => (
                y,
                self.b.map_or_else(
                    || Arc::<[PosedShape]>::from(Vec::new()),
                    |b| self.cache.shapes(b, y, self.counters),
                ),
            ),
            Choice::Blend(x, y) => {
                let sa = self.cache.shapes(self.a, x, self.counters);
                let sb = self.b.map_or_else(
                    || Arc::<[PosedShape]>::from(Vec::new()),
                    |b| self.cache.shapes(b, y, self.counters),
                );
                let blended: Vec<PosedShape> = sa
                    .iter()
                    .zip(sb.iter())
                    .map(|(p, q)| geom::blend(p, q, self.alpha))
                    .collect();
                self.counters.shape_buffers.fetch_add(1, Ordering::Relaxed);
                (x, Arc::<[PosedShape]>::from(blended))
            }
        };
        Posed {
            net_id: e.net_id,
            entity: e.entity,
            owner: e.owner,
            groups: e.groups.clone(),
            shapes,
            tags: e.tags.clone(),
        }
    }

    /// Every entity on either record (for the occlusion ignore list).
    fn entities(&self, out: &mut Vec<Entity>) {
        out.extend(self.a.entries.iter().map(|e| e.entity));
        if let Some(b) = self.b {
            out.extend(b.entries.iter().map(|e| e.entity));
        }
    }
}

/// One cast query's inputs.
struct Cast<'f> {
    kind: &'static str,
    shooter: Option<(Shooter, bool)>,
    tick: f64,
    ray: Ray,
    radius: f32,
    max_dist: f32,
    filter: &'f HitFilter,
}

/// Who is shooting: their `Player` entity and the peer whose entities they never hit.
#[derive(Clone, Copy)]
struct Shooter {
    id: NetPlayerId,
    player: Option<Entity>,
}

/// The read-only view a server system queries (spec 75 § 5): built from `&World`, so a
/// `fn system(world: &World)` runs in parallel with other readers.
pub struct LagCompensation<'w> {
    world: &'w World,
    history: &'w LagCompHistory,
    settings: LagCompSettings,
    mode: NetMode,
    current: u32,
    hz: f64,
    live: OnceCell<Arc<TickRecord>>,
    /// Shapes posed from recorded animation states (ADR-2551), shared by this view's queries.
    poses: PoseCache,
    /// Every `NetPlayer` as (id, entity, view tick), sorted; read once per view.
    players: OnceCell<Vec<(NetPlayerId, Entity, Option<f64>)>>,
    /// Every `NetOwner` as (peer, entity); read once per view, when occlusion needs it.
    owned: OnceCell<Vec<(PeerId, Entity)>>,
}

impl<'w> LagCompensation<'w> {
    /// The view over a server world.
    pub fn new(world: &'w World) -> Result<Self, LagCompError> {
        let mode = world.get_resource::<NetMode>().copied().unwrap_or_default();
        if !mode.is_server() {
            return Err(LagCompError::NotServer);
        }
        let history = world
            .get_resource::<LagCompHistory>()
            .ok_or(LagCompError::NotInstalled)?;
        let settings = world
            .get_resource::<NetSettings>()
            .map(|s| s.lag_compensation.clone())
            .unwrap_or_default();
        Ok(Self {
            world,
            history,
            settings,
            mode,
            current: world.get_resource::<NetNode>().map_or(0, |n| n.tick.0),
            hz: tick_hz(world),
            live: OnceCell::new(),
            poses: PoseCache::default(),
            players: OnceCell::new(),
            owned: OnceCell::new(),
        })
    }

    /// The server tick being simulated.
    pub fn current_tick(&self) -> NetTick {
        NetTick(self.current)
    }

    /// The oldest tick a query can pose (the oldest record, or the present).
    pub fn oldest_tick(&self) -> NetTick {
        NetTick(
            self.history
                .oldest_tick()
                .map_or(self.current, |t| t.0.min(self.current)),
        )
    }

    fn shooter(&self, id: NetPlayerId) -> Result<(Shooter, Option<f64>), LagCompError> {
        if id.peer != PeerId::SERVER
            && !self
                .world
                .get_resource::<NetNode>()
                .is_some_and(|n| n.has_peer(id.peer))
        {
            return Err(LagCompError::PeerUnknown);
        }
        let players = self.players.get_or_init(|| {
            let mut all: Vec<(NetPlayerId, Entity, Option<f64>)> = self
                .world
                .try_query::<(Entity, &NetPlayer, Option<&NetViewTick>)>()
                .map(|mut q| {
                    q.iter(self.world)
                        .map(|(e, p, v)| (p.id, e, v.map(|v| v.tick)))
                        .collect()
                })
                .unwrap_or_default();
            all.sort_unstable_by_key(|(id, e, _)| (*id, *e));
            all
        });
        // The first entity of the player (lowest `Entity`) when several carry the id.
        let i = players.partition_point(|(p, _, _)| *p < id);
        let (player, view) = match players.get(i) {
            Some((p, e, v)) if *p == id => (Some(*e), *v),
            _ => (None, None),
        };
        Ok((Shooter { id, player }, view))
    }

    /// `(tick, clamped)` for a shooter's reported view tick (spec 75 § 5 step 1).
    fn rewind_of(&self, shooter: NetPlayerId, view: Option<f64>) -> (f64, bool) {
        let now = f64::from(self.current);
        if !self.settings.enabled || self.mode == NetMode::Offline || shooter.peer == PeerId::SERVER {
            return (now, false);
        }
        let Some(v) = view.filter(|v| v.is_finite()) else {
            return (now, false);
        };
        let limit = now - f64::from(max_rewind_ticks(&self.settings, self.hz));
        let clamped = v < limit || v > now;
        let t = v.clamp(limit.max(f64::from(self.oldest_tick().0)), now);
        (t, clamped)
    }

    /// The tick a shooter's queries are posed at, after the clamp (spec 75 § 5 step 1).
    pub fn rewind_tick(&self, shooter: NetPlayerId) -> Result<NetTickFrac, LagCompError> {
        let (_, view) = self.shooter(shooter)?;
        Ok(NetTickFrac(self.rewind_of(shooter, view).0))
    }

    /// `fire_tick - rewind_tick(shooter)` in ticks: store it in [`LagCompProjectile`] when the
    /// projectile spawns.
    pub fn projectile_view_lag(&self, shooter: NetPlayerId) -> Result<f64, LagCompError> {
        let t = self.rewind_tick(shooter)?;
        Ok((f64::from(self.current) - t.0).max(0.0))
    }

    /// The tick a projectile's impact test runs at now (spec 75 § 7): the shooter's view during
    /// its first `projectile_rewind_ticks` ticks of flight, then the present.
    pub fn projectile_tick(&self, projectile: &LagCompProjectile) -> NetTickFrac {
        let now = f64::from(self.current);
        let flown = self.current.saturating_sub(projectile.fire_tick.0);
        if !self.settings.enabled || flown >= self.settings.projectile_rewind_ticks {
            return NetTickFrac(now);
        }
        let lag = if projectile.shooter_view_lag.is_finite() {
            projectile.shooter_view_lag.max(0.0)
        } else {
            0.0
        };
        let limit = now - f64::from(max_rewind_ticks(&self.settings, self.hz));
        NetTickFrac(
            (now - lag)
                .max(limit)
                .max(f64::from(self.oldest_tick().0))
                .min(now),
        )
    }

    fn log(&self, record: QueryRecord) {
        // Dev builds only (spec 75 § 11): a shipped server ignores the setting.
        if cfg!(debug_assertions) && self.settings.debug_echo {
            self.history.queue_echo(&record);
        }
        self.history.log_query(record);
    }

    /// The live pose of the present tick (built once per view, when a query needs it).
    ///
    /// The same record is staged so the following record of this tick does not pose it again.
    fn live(&self) -> &TickRecord {
        self.live
            .get_or_init(|| {
                // Lock only the staged slot, so the previous tick and the pending cuts stay
                // readable while the pose is built.
                let history = self.history;
                let mut staged = history.staged_live.lock().unwrap_or_else(|e| e.into_inner());
                let now = self.world.read_change_tick();
                if let Some(existing) = staged
                    .as_ref()
                    .filter(|live| live.rec.tick == self.current && live.at == now)
                {
                    Arc::clone(&existing.rec)
                } else {
                    let mut rec = TickRecord::default();
                    rec.clear(self.current);
                    let source = self.world.get_resource::<LagCompJointSource>().and_then(|s| s.0);
                    let prev = history.ticks.back();
                    let sources = Sources {
                        joints: source,
                        anim: self.world.get_resource::<LagCompAnimSource>().and_then(|s| s.0),
                        tick_secs: (1.0 / self.hz) as f32,
                    };
                    // A present-tick view has no shared cache (`live` only borrows the world).
                    // One walk per entity, same as a record with a cold cache.
                    let mut cache = OwnerCache::default();
                    let mut owners = OwnerLookup {
                        cache: &mut cache,
                        sig: (u64::MAX, u64::MAX),
                    };
                    pose_world(
                        self.world,
                        &mut rec,
                        &self.settings,
                        sources,
                        None,
                        prev,
                        &mut owners,
                    );
                    // A teleport on this tick has no record yet: its mark is still pending.
                    for e in &mut rec.entries {
                        e.cut = history.pending_cuts.contains(&e.entity);
                    }
                    let arc = Arc::new(rec);
                    *staged = Some(StagedLive {
                        at: now,
                        rec: Arc::clone(&arc),
                    });
                    arc
                }
            })
            .as_ref()
    }

    fn record_at(&self, tick: u32) -> Option<&TickRecord> {
        if tick == self.current && self.history.newest_tick().is_none_or(|t| t.0 < tick) {
            return Some(self.live());
        }
        let i = self.history.ticks.binary_search_by_key(&tick, |r| r.tick).ok()?;
        self.history.ticks.get(i)
    }

    fn bracket(&self, t: f64) -> Result<Bracket<'_>, LagCompError> {
        let now = f64::from(self.current);
        let oldest = f64::from(self.oldest_tick().0);
        if !t.is_finite() || t < oldest || t > now {
            return Err(LagCompError::TickOutOfHistory);
        }
        let a_tick = t.floor() as u32;
        let frac = t - f64::from(a_tick);
        let a = self.record_at(a_tick).ok_or(LagCompError::TickOutOfHistory)?;
        let b = if frac > 0.0 {
            self.record_at(a_tick + 1)
        } else {
            None
        };
        Ok(Bracket {
            a,
            b,
            alpha: if b.is_some() { frac as f32 } else { 0.0 },
            tick: t,
            cache: &self.poses,
            counters: &self.history.counters,
        })
    }

    /// Whether a candidate may be hit: not excluded, not the shooter's, passes groups and predicate.
    fn admits(&self, p: &Posed, filter: &HitFilter, shooter: Option<Shooter>) -> bool {
        if filter.exclude.contains(&p.entity) {
            return false;
        }
        if let Some(s) = shooter
            && (Some(p.entity) == s.player || p.owner == Some(s.id.peer))
        {
            return false;
        }
        if let Some(groups) = &filter.groups {
            let has = p
                .groups
                .as_ref()
                .is_some_and(|g| g.iter().any(|x| groups.0.contains(x)));
            if !has {
                return false;
            }
        }
        // A game's predicate is not asked about an entity that no longer exists.
        let exists = self.world.get_entity(p.entity).is_ok();
        !exists || filter.predicate.is_none_or(|f| f(self.world, p.entity))
    }

    fn alive(&self, p: &Posed) -> bool {
        self.world.get_entity(p.entity).is_ok_and(|e| {
            e.get::<HitShapes>().is_some() && e.get::<NetId>().is_some_and(|id| *id == p.net_id)
        })
    }

    fn hit_of(&self, p: &Posed, shape: usize, h: ShapeHit, tick: f64) -> LagHit {
        LagHit {
            entity: p.entity,
            net_id: p.net_id,
            tag: p.tags.get(shape).cloned().unwrap_or_default(),
            point: h.point,
            normal: h.normal,
            distance: h.distance,
            rewound_tick: NetTickFrac(tick),
            alive: self.alive(p),
        }
    }

    /// The occlusion hook's ignore list: every lag-compensated entity on the bracket and the
    /// shooter's own entities.
    fn ignore_list(&self, br: &Bracket<'_>, shooter: Option<Shooter>) -> Vec<Entity> {
        self.history.counters.ignore_lists.fetch_add(1, Ordering::Relaxed);
        let mut out = Vec::new();
        br.entities(&mut out);
        if let Some(s) = shooter {
            out.extend(s.player);
            // Its bodies with no hit shapes are not in the history, and a solid ray starts in them.
            let owned = self.owned.get_or_init(|| {
                self.world
                    .try_query::<(Entity, &NetOwner)>()
                    .map(|mut q| q.iter(self.world).map(|(e, o)| (o.0, e)).collect())
                    .unwrap_or_default()
            });
            out.extend(owned.iter().filter(|(p, _)| *p == s.id.peer).map(|(_, e)| *e));
            if let Some(newest) = self.history.ticks.back() {
                out.extend(
                    newest
                        .entries
                        .iter()
                        .filter(|e| e.owner == Some(s.id.peer))
                        .map(|e| e.entity),
                );
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// `ignore` is filled on the first call of a query and reused for the rest.
    fn occluded(
        &self,
        br: &Bracket<'_>,
        shooter: Option<Shooter>,
        ignore: &mut Option<Vec<Entity>>,
        ray: &Ray,
        dist: f32,
    ) -> bool {
        let Some(f) = self.world.get_resource::<LagCompOcclusion>().and_then(|o| o.0) else {
            return false;
        };
        let list = ignore.get_or_insert_with(|| self.ignore_list(br, shooter));
        f(self.world, ray, dist, list).is_some_and(|d| d < dist)
    }

    /// The nearest cast hit on the bracket (no occlusion), with the candidate it hit.
    fn nearest_cast(
        &self,
        br: &Bracket<'_>,
        ray: &Ray,
        radius: f32,
        max_dist: f32,
        filter: &HitFilter,
        shooter: Option<Shooter>,
    ) -> Option<CastMatch> {
        if ray.direction == Vec3::ZERO || max_dist.is_nan() || max_dist < 0.0 {
            return None;
        }
        let _span = tracing::trace_span!("net_lagcomp_query").entered();
        // An unbounded ray (`max_dist` infinite) spans every grid cell: the broad phase visits
        // every entry. Clamping its travel to 0 shrank the query to the ray origin, so a
        // single-record view missed every target away from it (corr-net, ADR-5580).
        let volume = if max_dist.is_finite() {
            let end = ray.origin + ray.direction * max_dist.max(0.0);
            Aabb {
                min: ray.origin.min(end),
                max: ray.origin.max(end),
            }
        } else {
            Aabb {
                min: Vec3::NEG_INFINITY,
                max: Vec3::INFINITY,
            }
        };
        let cands = br.candidates(radius, volume, |_, b| {
            ray.intersect_aabb(b).is_some_and(|t| t <= max_dist)
        });
        let mut best: Option<(f32, usize, usize, ShapeHit)> = None;
        for (ci, p) in cands.iter().enumerate() {
            if !self.admits(p, filter, shooter) {
                continue;
            }
            for (si, s) in p.shapes.iter().enumerate() {
                if let Some(h) = s.cast(ray, max_dist, radius)
                    && best.as_ref().is_none_or(|b| h.distance < b.0)
                {
                    best = Some((h.distance, ci, si, h));
                }
            }
        }
        let (_, ci, si, h) = best?;
        let p = &cands[ci];
        Some(CastMatch {
            hit: self.hit_of(p, si, h, br.tick),
            shapes: Arc::clone(&p.shapes),
            tags: Arc::clone(&p.tags),
        })
    }

    fn cast(&self, q: Cast<'_>) -> Result<Option<LagHit>, LagCompError> {
        let br = self.bracket(q.tick)?;
        let s = q.shooter.map(|(s, _)| s);
        let mut ignore = None;
        let mut hit = self.nearest_cast(&br, &q.ray, q.radius, q.max_dist, q.filter, s);
        if let Some(found) = &hit
            && self.occluded(&br, s, &mut ignore, &q.ray, found.hit.distance)
        {
            hit = None;
        }
        self.log(QueryRecord {
            kind: q.kind,
            shooter: s.map(|s| [s.id.peer.0, u32::from(s.id.local)]),
            server_tick: self.current,
            rewound_tick: q.tick,
            clamped: q.shooter.is_some_and(|(_, c)| c),
            origin: q.ray.origin.to_array(),
            direction: q.ray.direction.to_array(),
            radius: q.radius,
            hit: hit.as_ref().map(|found| HitReport::of(&found.hit)),
            shapes: hit
                .as_ref()
                .map(|found| shape_reports(&found.shapes, &found.tags))
                .unwrap_or_default(),
            verdict: None,
        });
        Ok(hit.map(|found| found.hit))
    }

    fn begin(&self, shooter: NetPlayerId) -> Result<(Shooter, f64, bool), LagCompError> {
        let (s, view) = self.shooter(shooter)?;
        let (t, clamped) = self.rewind_of(shooter, view);
        self.history
            .note_query(Some(shooter), f64::from(self.current) - t, clamped);
        Ok((s, t, clamped))
    }

    /// The nearest hit of `ray` within `max_dist` on the shapes as `shooter` saw them.
    pub fn raycast(
        &self,
        shooter: NetPlayerId,
        ray: Ray,
        max_dist: f32,
        filter: &HitFilter,
    ) -> Result<Option<LagHit>, LagCompError> {
        let (s, t, c) = self.begin(shooter)?;
        self.cast(Cast {
            kind: "raycast",
            shooter: Some((s, c)),
            tick: t,
            ray,
            radius: 0.0,
            max_dist,
            filter,
        })
    }

    /// The nearest hit of a sphere of `radius` swept along `ray`. Occlusion tests the center line.
    pub fn sphere_cast(
        &self,
        shooter: NetPlayerId,
        ray: Ray,
        radius: f32,
        max_dist: f32,
        filter: &HitFilter,
    ) -> Result<Option<LagHit>, LagCompError> {
        let (s, t, c) = self.begin(shooter)?;
        self.cast(Cast {
            kind: "sphere_cast",
            shooter: Some((s, c)),
            tick: t,
            ray,
            radius: radius.max(0.0),
            max_dist,
            filter,
        })
    }

    /// Every entity whose shapes touch the sphere, nearest shape per entity, in `NetId` order.
    /// A hit whose line from the center is blocked by static geometry is dropped.
    pub fn overlap_sphere(
        &self,
        shooter: NetPlayerId,
        center: Vec3,
        radius: f32,
        filter: &HitFilter,
    ) -> Result<Vec<LagHit>, LagCompError> {
        let (s, t, c) = self.begin(shooter)?;
        self.overlap(Some((s, c)), t, center, radius, filter)
    }

    fn overlap(
        &self,
        shooter: Option<(Shooter, bool)>,
        t: f64,
        center: Vec3,
        radius: f32,
        filter: &HitFilter,
    ) -> Result<Vec<LagHit>, LagCompError> {
        let br = self.bracket(t)?;
        let s = shooter.map(|(s, _)| s);
        let _span = tracing::trace_span!("net_lagcomp_query").entered();
        let probe = Aabb {
            min: center - Vec3::splat(radius),
            max: center + Vec3::splat(radius),
        };
        let cands = br.candidates(0.0, probe, |_, b| geom::overlaps(b, &probe));
        let mut out = Vec::new();
        let mut first_shapes = None;
        let mut ignore = None;
        for p in &cands {
            if !self.admits(p, filter, s) {
                continue;
            }
            let mut best: Option<(usize, ShapeHit)> = None;
            for (si, shape) in p.shapes.iter().enumerate() {
                if let Some(h) = shape.overlap_sphere(center, radius)
                    && best.as_ref().is_none_or(|b| h.distance < b.1.distance)
                {
                    best = Some((si, h));
                }
            }
            let Some((si, h)) = best else {
                continue;
            };
            let to = h.point - center;
            let len = to.length();
            if len > 1e-5 && self.occluded(&br, s, &mut ignore, &Ray::new(center, to), len - 1e-4) {
                continue;
            }
            if first_shapes.is_none() {
                first_shapes = Some(shape_reports(&p.shapes, &p.tags));
            }
            out.push(self.hit_of(p, si, h, br.tick));
        }
        self.log(QueryRecord {
            kind: "overlap_sphere",
            shooter: s.map(|s| [s.id.peer.0, u32::from(s.id.local)]),
            server_tick: self.current,
            rewound_tick: t,
            clamped: shooter.is_some_and(|(_, c)| c),
            origin: center.to_array(),
            direction: [0.0; 3],
            radius,
            hit: out.first().map(HitReport::of),
            shapes: first_shapes.unwrap_or_default(),
            verdict: None,
        });
        Ok(out)
    }

    /// The same queries at an explicit fractional tick, without a shooter.
    pub fn at_tick(&self, tick: NetTickFrac) -> Result<RewoundView<'_, 'w>, LagCompError> {
        self.bracket(tick.0)?;
        Ok(RewoundView {
            lc: self,
            tick: tick.0,
        })
    }

    /// Checks a client's hit claim (spec 75 § 8). `muzzle` is the shooter's muzzle point as the
    /// server computes it. Counts the verdict for the shooter.
    pub fn validate_claim(
        &self,
        shooter: NetPlayerId,
        claim: &HitClaim,
        muzzle: Vec3,
    ) -> Result<ClaimVerdict, LagCompError> {
        let (s, view) = self.shooter(shooter)?;
        let verdict = self.judge_claim(s, view, claim, muzzle);
        self.history
            .note_claim(shooter, verdict.as_ref().map(|_| ()).map_err(|r| *r));
        let ray = claim.ray();
        self.log(QueryRecord {
            kind: "claim",
            shooter: Some([shooter.peer.0, u32::from(shooter.local)]),
            server_tick: self.current,
            rewound_tick: claim.view_tick,
            clamped: false,
            origin: ray.origin.to_array(),
            direction: ray.direction.to_array(),
            radius: self.settings.claim_tolerance_m,
            hit: verdict.as_ref().ok().map(|(h, _)| HitReport::of(h)),
            shapes: verdict
                .as_ref()
                .ok()
                .map(|(_, sh)| sh.clone())
                .unwrap_or_default(),
            verdict: Some(match &verdict {
                Ok(_) => "valid",
                Err(r) => r.code(),
            }),
        });
        Ok(match verdict {
            Ok((h, _)) => ClaimVerdict::Valid(h),
            Err(r) => ClaimVerdict::Invalid(r),
        })
    }

    fn judge_claim(
        &self,
        s: Shooter,
        view: Option<f64>,
        claim: &HitClaim,
        muzzle: Vec3,
    ) -> Result<(LagHit, Vec<ShapeReport>), ClaimInvalid> {
        let now = f64::from(self.current);
        let limit = now - f64::from(max_rewind_ticks(&self.settings, self.hz));
        let vt = claim.view_tick;
        let reported = view.unwrap_or(now);
        if !vt.is_finite()
            || vt < limit
            || vt > now
            || (vt - reported).abs() > self.settings.claim_view_tick_slack
        {
            return Err(ClaimInvalid::ViewTickOutOfRange);
        }
        let gap = claim.origin.distance(muzzle);
        if gap.is_nan() || gap > self.settings.claim_origin_tolerance_m {
            return Err(ClaimInvalid::OriginTooFar);
        }
        // Without rewind the claim is judged at the present, like a server-traced shot.
        let t = if self.settings.enabled { vt } else { now };
        let br = self.bracket(t).map_err(|_| ClaimInvalid::ViewTickOutOfRange)?;
        // The target is named, not boxed. A non-finite volume visits every entry.
        let anywhere = Aabb {
            min: Vec3::NAN,
            max: Vec3::NAN,
        };
        let target = br
            .candidates(0.0, anywhere, |e, _| e == claim.target)
            .into_iter()
            .next()
            .filter(|p| Some(p.entity) != s.player && p.owner != Some(s.id.peer))
            .ok_or(ClaimInvalid::TargetUnknown)?;
        // `Ray::new` gives a zero direction for one that was zero or not finite.
        let ray = claim.ray();
        if ray.direction == Vec3::ZERO {
            return Err(ClaimInvalid::MissedShape);
        }
        let tol = self.settings.claim_tolerance_m.max(0.0);
        let mut best: Option<(usize, ShapeHit)> = None;
        for (si, shape) in target.shapes.iter().enumerate() {
            if target.tags.get(si).is_none_or(|t| t.as_str() != claim.tag) {
                continue;
            }
            if let Some(h) = shape.cast(&ray, f32::MAX, tol)
                && best.as_ref().is_none_or(|b| h.distance < b.1.distance)
            {
                best = Some((si, h));
            }
        }
        let (si, h) = best.ok_or(ClaimInvalid::MissedShape)?;
        let mut ignore = None;
        if self.occluded(&br, Some(s), &mut ignore, &ray, h.distance) {
            return Err(ClaimInvalid::Occluded);
        }
        Ok((
            self.hit_of(&target, si, h, t),
            shape_reports(&target.shapes, &target.tags),
        ))
    }
}

/// The queries at a fixed tick, without a shooter (no shooter exclusion, no clamp): projectile
/// impacts and replays of claims. Built by [`LagCompensation::at_tick`].
pub struct RewoundView<'v, 'w> {
    lc: &'v LagCompensation<'w>,
    tick: f64,
}

impl RewoundView<'_, '_> {
    /// The tick the shapes are posed at.
    pub fn tick(&self) -> NetTickFrac {
        NetTickFrac(self.tick)
    }

    /// The nearest hit of `ray` within `max_dist`.
    pub fn raycast(
        &self,
        ray: Ray,
        max_dist: f32,
        filter: &HitFilter,
    ) -> Result<Option<LagHit>, LagCompError> {
        self.note();
        self.lc.cast(Cast {
            kind: "raycast",
            shooter: None,
            tick: self.tick,
            ray,
            radius: 0.0,
            max_dist,
            filter,
        })
    }

    /// The nearest hit of a swept sphere.
    pub fn sphere_cast(
        &self,
        ray: Ray,
        radius: f32,
        max_dist: f32,
        filter: &HitFilter,
    ) -> Result<Option<LagHit>, LagCompError> {
        self.note();
        self.lc.cast(Cast {
            kind: "sphere_cast",
            shooter: None,
            tick: self.tick,
            ray,
            radius: radius.max(0.0),
            max_dist,
            filter,
        })
    }

    /// Every entity touching the sphere.
    pub fn overlap_sphere(
        &self,
        center: Vec3,
        radius: f32,
        filter: &HitFilter,
    ) -> Result<Vec<LagHit>, LagCompError> {
        self.note();
        self.lc.overlap(None, self.tick, center, radius, filter)
    }

    fn note(&self) {
        self.lc
            .history
            .note_query(None, f64::from(self.lc.current) - self.tick, false);
    }
}

// ---- agent data ---------------------------------------------------------------------------------

/// Why an agent read of the history failed (`book/agents/net-lag-compensation.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LagCompQueryError {
    /// A query error (`net_not_server`, `lagcomp_not_installed`, `peer_unknown`,
    /// `tick_out_of_history`).
    Query(LagCompError),
    /// The entity has no recorded history: `entity_not_lag_compensated`.
    NotLagCompensated,
}

impl LagCompQueryError {
    /// The protocol error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Query(e) => e.code(),
            Self::NotLagCompensated => "entity_not_lag_compensated",
        }
    }
}

impl From<LagCompError> for LagCompQueryError {
    fn from(e: LagCompError) -> Self {
        Self::Query(e)
    }
}

/// `lagcomp.stats`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LagCompStats {
    /// Lag-compensated entities on the newest recorded tick.
    pub entities: usize,
    /// Ticks held.
    pub ticks_kept: usize,
    /// Oldest and newest recorded ticks.
    pub range: Option<[u32; 2]>,
    /// The rewind window in ticks.
    pub max_rewind_ticks: u32,
    /// Queries over the last second of ticks.
    pub queries_per_s: u64,
    /// Mean rewind of all queries, in milliseconds.
    pub mean_rewind_ms: f64,
    /// Clamped view ticks over the last second.
    pub clamped_per_s: u64,
    /// Valid claims so far.
    pub claims_valid: u64,
    /// Refused claims so far, by reason code.
    pub claims_invalid_by_reason: BTreeMap<&'static str, u64>,
    /// Whether rewind is on (`lag_compensation.enabled`).
    pub enabled: bool,
    /// Whether an occlusion hook is installed.
    pub occlusion: bool,
    /// Whether a joint pose hook is installed.
    pub joint_source: bool,
}

fn server_history(world: &World) -> Result<&LagCompHistory, LagCompError> {
    if !world.get_resource::<NetMode>().is_some_and(|m| m.is_server()) {
        return Err(LagCompError::NotServer);
    }
    world
        .get_resource::<LagCompHistory>()
        .ok_or(LagCompError::NotInstalled)
}

/// Data of `lagcomp.stats` (server).
pub fn stats_report(world: &World) -> Result<LagCompStats, LagCompError> {
    let h = server_history(world)?;
    let settings = world
        .get_resource::<NetSettings>()
        .map(|s| s.lag_compensation.clone())
        .unwrap_or_default();
    let hz = tick_hz(world);
    let (_, _, valid, invalid) = h.totals();
    let (qps, cps) = h.per_second();
    Ok(LagCompStats {
        entities: h.entities(),
        ticks_kept: h.ticks_kept(),
        range: h.oldest_tick().zip(h.newest_tick()).map(|(a, b)| [a.0, b.0]),
        max_rewind_ticks: max_rewind_ticks(&settings, hz),
        queries_per_s: qps,
        mean_rewind_ms: h.mean_rewind_ticks() * 1000.0 / hz,
        clamped_per_s: cps,
        claims_valid: valid,
        claims_invalid_by_reason: ClaimInvalid::ALL
            .iter()
            .map(|r| (r.code(), invalid[r.index()]))
            .collect(),
        enabled: settings.enabled,
        occlusion: world
            .get_resource::<LagCompOcclusion>()
            .is_some_and(|o| o.0.is_some()),
        joint_source: world
            .get_resource::<LagCompJointSource>()
            .is_some_and(|o| o.0.is_some()),
    })
}

/// One recorded tick of one entity (`lagcomp.history`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryTick {
    /// The tick.
    pub tick: u32,
    /// The entity teleported on this tick.
    pub cut: bool,
    /// Its shapes.
    pub shapes: Vec<ShapeReport>,
}

/// Data of `lagcomp.history` (server).
pub fn history_report(world: &World, entity: Entity) -> Result<Vec<HistoryTick>, LagCompQueryError> {
    let h = server_history(world)?;
    let rows = h.history_of(entity);
    if rows.is_empty() {
        return Err(LagCompQueryError::NotLagCompensated);
    }
    Ok(rows
        .into_iter()
        .map(|(t, cut, shapes)| HistoryTick {
            tick: t.0,
            cut,
            shapes,
        })
        .collect())
}

/// The hit shapes of one entity at one rewound tick (`lagcomp.rewound_shapes`, dev builds).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RewoundShapes {
    /// Entity bits.
    pub entity: u64,
    /// Its `NetId`.
    pub net_id: u32,
    /// The tick the shapes are posed at.
    pub tick: f64,
    /// `anim_state` when posed from the recorded animation state of the ticks around `tick`
    /// (ADR-2551), `recorded` when from recorded shape poses.
    pub source: &'static str,
    /// Whether the entity still exists.
    pub alive: bool,
    /// The shapes, in definition order.
    pub shapes: Vec<ShapeReport>,
}

impl LagCompensation<'_> {
    /// The hit shapes of `entity` posed at `tick`, exactly as a rewound query at that tick tests
    /// them (the same bracket, blend and teleport rules).
    ///
    /// # Errors
    ///
    /// `tick_out_of_history` outside the kept range; `entity_not_lag_compensated` when the
    /// entity has no record around the tick.
    pub fn rewound_shapes(
        &self,
        entity: Entity,
        tick: NetTickFrac,
    ) -> Result<RewoundShapes, LagCompQueryError> {
        let br = self.bracket(tick.0)?;
        let anim =
            br.a.entries
                .iter()
                .chain(br.b.iter().flat_map(|b| b.entries.iter()))
                .any(|e| e.entity == entity && e.anim.is_some());
        // The entity is named, not boxed. A non-finite volume visits every entry.
        let anywhere = Aabb {
            min: Vec3::NAN,
            max: Vec3::NAN,
        };
        let posed = br
            .candidates(0.0, anywhere, |e, _| e == entity)
            .into_iter()
            .next()
            .ok_or(LagCompQueryError::NotLagCompensated)?;
        Ok(RewoundShapes {
            entity: entity.to_bits(),
            net_id: posed.net_id.0,
            tick: br.tick,
            source: if anim { "anim_state" } else { "recorded" },
            alive: self.alive(&posed),
            shapes: shape_reports(&posed.shapes, &posed.tags),
        })
    }
}

/// Data of `lagcomp.rewound_shapes` (server): [`LagCompensation::rewound_shapes`].
///
/// # Errors
///
/// As [`LagCompensation::new`] and [`LagCompensation::rewound_shapes`].
pub fn rewound_shapes_report(
    world: &World,
    entity: Entity,
    tick: f64,
) -> Result<RewoundShapes, LagCompQueryError> {
    LagCompensation::new(world)?.rewound_shapes(entity, NetTickFrac(tick))
}

/// Data of `lagcomp.last_queries` (server): the newest `limit` queries, oldest first.
pub fn last_queries_report(world: &World, limit: usize) -> Result<Vec<QueryRecord>, LagCompError> {
    let h = server_history(world)?;
    let all = h.last_queries();
    let skip = all.len().saturating_sub(limit);
    Ok(all.into_iter().skip(skip).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::resolve_owner;

    /// Spec 78 § 12.2, ADR-2022: a local rebase moves every recorded box and shape.
    #[test]
    fn a_rebase_moves_the_recorded_history() {
        let mut history = LagCompHistory::default();
        let at = Vec3::new(4100.0, 1.0, 0.0);
        history.ticks.push_back(TickRecord {
            tick: 3,
            entries: vec![Entry {
                net_id: NetId(1),
                entity: Entity::PLACEHOLDER,
                owner: None,
                groups: None,
                cut: false,
                aabb: Aabb {
                    min: at - Vec3::ONE,
                    max: at + Vec3::ONE,
                },
                swing: 0.0,
                start: 0,
                len: 1,
                tags: Arc::from(Vec::new()),
                anim: None,
            }],
            shapes: vec![PosedShape {
                kind: HitShapeKind::Sphere { radius: 0.5 },
                position: at,
                rotation: Quat::IDENTITY,
            }],
            ..TickRecord::default()
        });
        history.shift_origin(Vec3::new(-4096.0, 0.0, 0.0));
        let record = &history.ticks[0];
        assert_eq!(record.entries[0].aabb.min, Vec3::new(3.0, 0.0, -1.0));
        assert_eq!(record.entries[0].aabb.max, Vec3::new(5.0, 2.0, 1.0));
        assert_eq!(record.shapes[0].position, Vec3::new(4.0, 1.0, 0.0));
    }

    #[test]
    fn rewind_window_in_ticks() {
        let s = LagCompSettings {
            max_rewind_ms: 250.0,
            ..LagCompSettings::default()
        };
        assert_eq!(max_rewind_ticks(&s, 60.0), 15);
        assert_eq!(max_rewind_ticks(&s, 30.0), 8);
        // Spec 120 § 9.6: the default project maximum is 1,000 ms, and no project goes past it.
        assert_eq!(max_rewind_ticks(&LagCompSettings::default(), 60.0), 60);
        let over = LagCompSettings {
            max_rewind_ms: 5_000.0,
            ..LagCompSettings::default()
        };
        assert_eq!(max_rewind_ticks(&over, 60.0), 60);
        let zero = LagCompSettings {
            max_rewind_ms: f64::NAN,
            ..LagCompSettings::default()
        };
        assert_eq!(max_rewind_ticks(&zero, 60.0), 0);
    }

    #[test]
    fn error_codes_are_protocol_codes() {
        let codes = [
            LagCompError::NotServer.code(),
            LagCompError::NotInstalled.code(),
            LagCompError::PeerUnknown.code(),
            LagCompError::TickOutOfHistory.code(),
            LagCompQueryError::NotLagCompensated.code(),
        ]
        .into_iter()
        .chain(ClaimInvalid::ALL.iter().map(|r| r.code()));
        for c in codes {
            assert!(
                c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{c}"
            );
        }
    }

    #[test]
    fn invalid_shapes_are_rejected() {
        assert!(HitShapeKind::Sphere { radius: 0.3 }.is_valid());
        assert!(!HitShapeKind::Sphere { radius: 0.0 }.is_valid());
        assert!(!HitShapeKind::Sphere { radius: f32::NAN }.is_valid());
        assert!(
            HitShapeKind::Capsule {
                radius: 0.3,
                half_height: 0.0
            }
            .is_valid()
        );
        assert!(
            !HitShapeKind::Box {
                half_extents: Vec3::new(1.0, -1.0, 1.0)
            }
            .is_valid()
        );
    }

    // ---- exact-tick joints (ADR-2551) ----

    /// A rig whose head joint sits `y` above the root; it knows no other joint.
    #[derive(Debug)]
    struct FakeAnim {
        y: f32,
    }

    impl RecordedAnim for FakeAnim {
        fn joint_poses(&self, root: &Affine3A, joints: &[&str], out: &mut Vec<Option<Affine3A>>) -> bool {
            out.clear();
            out.extend(joints.iter().map(|j| {
                (*j == "head").then(|| *root * Affine3A::from_translation(Vec3::new(0.0, self.y, 0.0)))
            }));
            true
        }
        fn has_joint(&self, joint: &str) -> bool {
            joint == "head"
        }
        fn reach(&self) -> f32 {
            2.0
        }
        fn heap_bytes(&self) -> usize {
            std::mem::size_of::<Self>()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// The head height a test drives per tick.
    #[derive(Component)]
    struct HeadY(f32);

    fn fake_capture(
        world: &World,
        e: Entity,
        _seconds: f32,
        _previous: Option<&Arc<dyn RecordedAnim>>,
    ) -> Result<Arc<dyn RecordedAnim>, &'static str> {
        let y = world.get::<HeadY>(e).ok_or("no_rig")?.0;
        Ok(Arc::new(FakeAnim { y }))
    }

    fn anim_server() -> (World, Entity) {
        let mut w = World::new();
        w.insert_resource(NetMode::DedicatedServer);
        crate::runtime::ensure_resources(&mut w);
        install_lag_compensation(&mut w);
        w.resource_mut::<LagCompAnimSource>().0 = Some(fake_capture);
        let e = w
            .spawn((
                NetId(1),
                Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
                HeadY(1.0),
                HitShapes {
                    shapes: vec![
                        HitShape::new(HitShapeKind::Sphere { radius: 0.3 }, "body"),
                        HitShape::new(HitShapeKind::Sphere { radius: 0.1 }, "head").on_joint("head"),
                        HitShape::new(HitShapeKind::Sphere { radius: 0.1 }, "tail").on_joint("tail"),
                    ],
                    pose_source: PoseSource::Joints,
                },
            ))
            .id();
        (w, e)
    }

    fn record_to(w: &mut World, e: Entity, last: u32) {
        for t in 0..=last {
            if let Some(mut y) = w.get_mut::<HeadY>(e) {
                y.0 = 1.0 + 0.1 * t as f32;
            }
            w.resource_mut::<NetNode>().tick = NetTick(t);
            lagcomp_record(w);
        }
        w.resource_mut::<NetNode>().tick = NetTick(last + 1);
    }

    /// The history keeps the state, poses joints from it at the exact tick (blending between two),
    /// a missing joint follows the root and warns once, and a query hits the joint shape there.
    #[test]
    fn joint_shapes_pose_from_the_recorded_state() {
        let (mut w, e) = anim_server();
        record_to(&mut w, e, 10);
        let h = w.resource::<LagCompHistory>();
        assert_eq!(h.anim_entities(), 1);
        assert_eq!(h.joint_missing_logged, 1, "tail is not a joint of the rig");
        let lc = LagCompensation::new(&w).expect("server");
        let at = |t: f64| {
            let r = lc.rewound_shapes(e, NetTickFrac(t)).expect("rewound");
            assert_eq!(r.source, "anim_state");
            r.shapes
                .iter()
                .map(|s| Vec3::from_array(s.position))
                .collect::<Vec<_>>()
        };
        let s5 = at(5.0);
        assert_eq!(s5[0], Vec3::new(0.0, 0.0, 10.0), "a root shape");
        assert!(
            (s5[1] - Vec3::new(0.0, 1.5, 10.0)).length() < 1e-6,
            "head at tick 5: {}",
            s5[1]
        );
        assert_eq!(
            s5[2],
            Vec3::new(0.0, 0.0, 10.0),
            "a missing joint follows the root"
        );
        let half = at(5.5);
        assert!(
            (half[1].y - 1.55).abs() < 1e-5,
            "between two exact ticks: {}",
            half[1]
        );
        let view = lc.at_tick(NetTickFrac(5.0)).expect("view");
        let hit = view
            .raycast(
                Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::Z),
                100.0,
                &HitFilter::default(),
            )
            .expect("query")
            .expect("hit");
        assert_eq!(hit.tag.as_str(), "head");
        assert!((hit.distance - 9.9).abs() < 1e-4, "{}", hit.distance);
        let miss = view
            .raycast(
                Ray::new(Vec3::new(0.0, 1.0, 0.0), Vec3::Z),
                100.0,
                &HitFilter::default(),
            )
            .expect("query");
        assert!(miss.is_none(), "the head was not at tick 0's height on tick 5");
        let (poses, failed, outside) = w.resource::<LagCompHistory>().anim_counters();
        assert!(
            poses >= 1 && failed == 0 && outside == 0,
            "{poses} {failed} {outside}"
        );
    }

    /// A capture refusal falls back to posed shapes for that tick, counted by code; a pose outside
    /// the reach box is counted.
    #[test]
    fn refusals_fall_back_and_reach_is_checked() {
        let (mut w, e) = anim_server();
        record_to(&mut w, e, 3);
        w.entity_mut(e).remove::<HeadY>();
        w.resource_mut::<NetNode>().tick = NetTick(4);
        lagcomp_record(&mut w);
        let h = w.resource::<LagCompHistory>();
        assert_eq!(h.anim_fallbacks().get("no_rig"), Some(&1));
        assert_eq!(h.anim_entities(), 0, "tick 4 is posed shapes");
        w.entity_mut(e).insert(HeadY(9.0));
        w.resource_mut::<NetNode>().tick = NetTick(5);
        lagcomp_record(&mut w);
        w.resource_mut::<NetNode>().tick = NetTick(6);
        let lc = LagCompensation::new(&w).expect("server");
        let r = lc.rewound_shapes(e, NetTickFrac(4.0)).expect("tick 4");
        assert_eq!(r.source, "recorded");
        let r = lc.rewound_shapes(e, NetTickFrac(5.0)).expect("tick 5");
        assert_eq!(r.source, "anim_state");
        assert_eq!(
            w.resource::<LagCompHistory>().anim_counters().2,
            1,
            "a head 9 m up is past the 2 m reach"
        );
        // Bytes per tick: the box plus the state, no posed shapes.
        let bytes = w.resource::<LagCompHistory>().entity_bytes(e);
        assert!(bytes > 0 && bytes < 6 * 400, "{bytes}");
    }

    fn body(source: PoseSource) -> HitShapes {
        HitShapes {
            shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.4 }, "body")],
            pose_source: source,
        }
    }

    fn recorded_owner(world: &World, entity: Entity) -> Option<PeerId> {
        world
            .resource::<LagCompHistory>()
            .ticks
            .back()
            .and_then(|r| r.entries.iter().find(|e| e.entity == entity))
            .map(|e| e.owner)
            .expect("recorded")
    }

    fn owner_bit(world: &World, entity: Entity) -> Option<PeerId> {
        match resolve_owner(world, entity) {
            OwnerResolution::Owned(p) => Some(p),
            _ => None,
        }
    }

    /// Recorded owners match `resolve_owner` for an owned chain, a server-owned shape, a broken
    /// chain and a joint capture. A second record with the same ownership inputs does not walk
    /// again; editing `NetOwner` does.
    #[test]
    fn a_second_record_does_not_rewalk_a_stable_owner() {
        use crate::model::{NetOwnerLink, owner_walks, reset_owner_walks};
        use crate::runtime::ensure_resources;

        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        world.resource_mut::<LagCompAnimSource>().0 = Some(fake_capture);

        let pawn = world
            .spawn((
                NetId(1),
                NetOwner(PeerId(4)),
                Transform::IDENTITY,
                body(PoseSource::Root),
            ))
            .id();
        let child = world
            .spawn((
                NetId(2),
                ChildOf(pawn),
                Transform::IDENTITY,
                body(PoseSource::Root),
            ))
            .id();
        let loose = world
            .spawn((NetId(3), Transform::IDENTITY, body(PoseSource::Root)))
            .id();
        let a = world.spawn_empty().id();
        let b = world.spawn(NetOwnerLink(a)).id();
        let broken = world
            .spawn((
                NetId(4),
                NetOwnerLink(b),
                Transform::IDENTITY,
                body(PoseSource::Root),
            ))
            .id();
        world.entity_mut(a).insert(NetOwnerLink(broken));
        let joints = world
            .spawn((
                NetId(5),
                NetOwner(PeerId(6)),
                Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
                HeadY(1.0),
                body(PoseSource::Joints),
            ))
            .id();

        reset_owner_walks();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        let first = owner_walks();
        assert!(first >= 5, "the first record walks each shape ({first})");
        for e in [pawn, child, loose, broken, joints] {
            assert_eq!(recorded_owner(&world, e), owner_bit(&world, e), "{e:?}");
        }
        assert_eq!(recorded_owner(&world, pawn), Some(PeerId(4)));
        assert_eq!(recorded_owner(&world, child), Some(PeerId(4)));
        assert_eq!(recorded_owner(&world, loose), None);
        assert_eq!(recorded_owner(&world, broken), None);
        assert_eq!(recorded_owner(&world, joints), Some(PeerId(6)));

        reset_owner_walks();
        world.resource_mut::<NetNode>().tick = NetTick(2);
        lagcomp_record(&mut world);
        let second = owner_walks();
        assert_eq!(second, 0, "an unchanged signature does not walk ({second})");
        assert_eq!(recorded_owner(&world, child), Some(PeerId(4)));
        assert_eq!(recorded_owner(&world, joints), Some(PeerId(6)));

        world.entity_mut(pawn).insert(NetOwner(PeerId(8)));
        reset_owner_walks();
        world.resource_mut::<NetNode>().tick = NetTick(3);
        lagcomp_record(&mut world);
        let third = owner_walks();
        assert!(third >= 5, "a new owner signature walks again ({third})");
        assert_eq!(recorded_owner(&world, pawn), Some(PeerId(8)));
        assert_eq!(recorded_owner(&world, child), Some(PeerId(8)));
        assert_eq!(recorded_owner(&world, joints), Some(PeerId(6)));
    }

    /// A non-finite hit-shape rotation is not a pose. `rot_trans` already substitutes
    /// identity; the posed-shape and animation-state paths must do the same, or one NaN
    /// component poisons every later rewind.
    #[test]
    fn non_finite_hit_rotation_records_as_identity() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        crate::runtime::ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        let mut shape = HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body");
        shape.offset.rotation = Quat::from_array([f32::NAN, 0.0, 0.0, 1.0]);
        let e = world
            .spawn((
                NetId(1),
                Transform::IDENTITY,
                HitShapes {
                    shapes: vec![shape],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        let report = world
            .resource::<LagCompHistory>()
            .newest_shapes_of(e)
            .expect("one recorded shape");
        assert_eq!(report.len(), 1);
        assert!(
            report[0].rotation.iter().all(|c| c.is_finite()),
            "a NaN offset rotation poisoned the recorded pose: {:?}",
            report[0].rotation
        );
        assert_eq!(report[0].rotation, Quat::IDENTITY.to_array());

        let mut offset = HitShape::new(HitShapeKind::Sphere { radius: 0.3 }, "body");
        offset.offset.rotation = Quat::from_array([f32::INFINITY, 0.0, 0.0, 0.0]);
        let anim = AnimEntry {
            state: Arc::new(FakeAnim { y: 0.0 }),
            rotation: Quat::IDENTITY,
            translation: Vec3::ZERO,
            scale: 1.0,
            shapes: Arc::from([offset]),
        };
        let mut posed = Vec::new();
        assert!(anim.pose(&mut posed));
        assert_eq!(posed.len(), 1);
        assert!(
            posed[0].rotation.is_finite(),
            "posing a recorded state kept a non-finite rotation: {:?}",
            posed[0].rotation
        );
        assert_eq!(posed[0].rotation, Quat::IDENTITY);

        let root = Affine3A::from_scale_rotation_translation(
            Vec3::ONE,
            Quat::from_array([f32::NAN, 0.0, 0.0, 1.0]),
            Vec3::ZERO,
        );
        let hs = HitShapes {
            shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.3 }, "body")],
            pose_source: PoseSource::Joints,
        };
        let entity = world.spawn_empty().id();
        let state: Arc<dyn RecordedAnim> = Arc::new(FakeAnim { y: 0.0 });
        let sig = crate::replication::owner_record_signature(&mut world);
        let mut cache = OwnerCache::default();
        let mut owners = OwnerLookup {
            cache: &mut cache,
            sig,
        };
        let entry = anim_entry(
            &world,
            AnimInputs {
                entity,
                net_id: NetId(1),
                hs: &hs,
                root,
                state,
            },
            &LagCompSettings::default(),
            None,
            None,
            &mut owners,
        )
        .expect("a valid shape still records");
        let stored = entry.anim.expect("animation state").rotation;
        assert!(
            stored.is_finite(),
            "a NaN root rotation was stored on the animation entry: {stored:?}"
        );
        assert_eq!(stored, Quat::IDENTITY);
    }

    /// corr-net (ADR-5580): a hit claim judged at the present tick poses the world before that
    /// tick's movement (`NetSet::Apply` runs before gameplay). The record at the end of the tick
    /// must pose the world as it is then, not adopt the query's earlier pose: every later rewind
    /// to that tick would test one-tick-old shapes.
    #[test]
    fn corr_net_a_present_tick_query_does_not_freeze_a_stale_record() {
        use crate::runtime::ensure_resources;

        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        let e = world
            .spawn((NetId(1), Transform::IDENTITY, body(PoseSource::Root)))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        world.resource_mut::<NetNode>().tick = NetTick(2);
        {
            let lc = LagCompensation::new(&world).expect("view");
            let view = lc.at_tick(NetTickFrac(2.0)).expect("present tick");
            let hit = view
                .raycast(
                    Ray::new(Vec3::new(0.0, 0.0, -5.0), Vec3::Z),
                    100.0,
                    &HitFilter::default(),
                )
                .expect("query");
            assert!(hit.is_some(), "the shape is at the origin before the move");
        }
        // Gameplay runs after the claim (a system boundary), then moves the entity.
        world.increment_change_tick();
        world
            .entity_mut(e)
            .insert(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        world.increment_change_tick();
        lagcomp_record(&mut world);
        let shapes = world
            .resource::<LagCompHistory>()
            .newest_shapes_of(e)
            .expect("recorded");
        assert_eq!(
            shapes[0].position,
            [10.0, 0.0, 0.0],
            "tick 2 records where the entity is at the end of the tick"
        );
    }

    /// corr-net (ADR-5580): an unbounded ray (`max_dist` infinite, or an agent `max_dist` that
    /// overflows `f32`) hits a target 50 m away on an exact-tick view, as a 100 m ray does. The
    /// broad phase used a zero-length query box at the ray origin and found nothing.
    #[test]
    fn corr_net_an_unbounded_ray_reaches_distant_targets() {
        use crate::runtime::ensure_resources;

        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        world.spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(0.0, 0.0, 50.0)),
            body(PoseSource::Root),
        ));
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        world.resource_mut::<NetNode>().tick = NetTick(2);
        lagcomp_record(&mut world);
        let lc = LagCompensation::new(&world).expect("view");
        let view = lc.at_tick(NetTickFrac(1.0)).expect("an exact recorded tick");
        let cast = |max_dist: f32| {
            view.raycast(Ray::new(Vec3::ZERO, Vec3::Z), max_dist, &HitFilter::default())
                .expect("query")
                .map(|h| h.distance)
        };
        let bounded = cast(100.0).expect("a 100 m ray hits");
        assert_eq!(
            cast(f32::INFINITY),
            Some(bounded),
            "an unbounded ray hits the same shape"
        );
        assert_eq!(cast(10.0), None, "a 10 m ray stops short");
    }

    /// corr-net (ADR-5580): two children that swap parents change their owners. The owner cache
    /// key summed `child ^ rotl(parent, 17)`, which is the same after the swap whenever the bits
    /// do not overlap (small entity indices), so the record kept the old owners.
    #[test]
    fn corr_net_children_that_swap_parents_record_their_new_owners() {
        use crate::runtime::ensure_resources;

        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        let p1 = world
            .spawn((NetId(1), NetOwner(PeerId(4)), Transform::IDENTITY))
            .id();
        let p2 = world
            .spawn((NetId(2), NetOwner(PeerId(5)), Transform::IDENTITY))
            .id();
        let c1 = world
            .spawn((NetId(3), ChildOf(p1), Transform::IDENTITY, body(PoseSource::Root)))
            .id();
        let c2 = world
            .spawn((NetId(4), ChildOf(p2), Transform::IDENTITY, body(PoseSource::Root)))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        assert_eq!(recorded_owner(&world, c1), Some(PeerId(4)));
        assert_eq!(recorded_owner(&world, c2), Some(PeerId(5)));
        world.entity_mut(c1).insert(ChildOf(p2));
        world.entity_mut(c2).insert(ChildOf(p1));
        world.resource_mut::<NetNode>().tick = NetTick(2);
        lagcomp_record(&mut world);
        assert_eq!(recorded_owner(&world, c1), owner_bit(&world, c1));
        assert_eq!(
            recorded_owner(&world, c1),
            Some(PeerId(5)),
            "c1 now hangs under p2"
        );
        assert_eq!(
            recorded_owner(&world, c2),
            Some(PeerId(4)),
            "c2 now hangs under p1"
        );
    }
}

#[cfg(test)]
mod det_tests {
    //! Cross-platform pose determinism (spec 71 § 12): the recorded and blended hit-shape
    //! rotations must be bit-identical on x86_64 and aarch64, so every normalization and dot
    //! product on this path must be the scalar left-to-right `normalize4`/`quat_dot`, never
    //! glam's SIMD dot4. glam's SSE2 dot4 keeps the `(x + z) + (y + w)` grouping, NEON's
    //! `vaddvq_f32` reduces pairwise `(x + y) + (z + w)`, and the scalar order is
    //! `((x + y) + z) + w`; `ADVERSARIAL` below is a quaternion whose sum of squares rounds
    //! differently under all three orderings (found by search over random bit patterns; the same
    //! search also drives the `rot_trans` and `interp::nlerp` tests).
    use super::*;
    use crate::runtime::ensure_resources;

    /// Four components whose sum of squares rounds to three different bit patterns under the
    /// three horizontal-sum orderings (SSE2 `(x + z) + (y + w)`, NEON pairwise `(x + y) + (z + w)`,
    /// scalar left-to-right), so reverting any site to glam's SIMD normalize fails on x86_64
    /// *and* aarch64 (found by search over random bit patterns).
    const ADVERSARIAL: [f32; 4] = [0.396_278_26, 0.367_807_63, 0.041_129_23, 0.353_028_54];

    /// A unit quaternion whose Shepperd reconstruction (what `to_scale_rotation_translation`
    /// extracts from the matrix) is itself adversarial: normalizing it rounds differently under
    /// SSE2 pairing than left-to-right (found by search).
    const ADVERSARIAL_ROT: [f32; 4] = [0.296_132_3, 0.499_176_65, -0.431_205_9, 0.690_789_34];

    /// The independent reference: one left-to-right sum of squares, one `sqrt`, one division per
    /// component. Written out again (not imported from `crate::scalar`) so the test pins the
    /// order rather than tautologically calling the production helper.
    fn lr_normalize(q: [f32; 4]) -> [f32; 4] {
        let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
        let len = len2.sqrt();
        [q[0] / len, q[1] / len, q[2] / len, q[3] / len]
    }

    fn bits(q: Quat) -> [u32; 4] {
        q.to_array().map(f32::to_bits)
    }

    /// A server world with one root-posed hit shape whose offset rotation is `ADVERSARIAL`, so
    /// the recorded pose normalizes exactly the adversarial quaternion (the root is the
    /// identity, and a quaternion product with the identity is per-component exact).
    fn recorded_rotation(world: &mut World, shape_rot: Quat) -> [u32; 4] {
        let mut shape = HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body");
        shape.offset.rotation = shape_rot;
        let e = world
            .spawn((
                NetId(1),
                Transform::IDENTITY,
                HitShapes {
                    shapes: vec![shape],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(world);
        let report = world
            .resource::<LagCompHistory>()
            .newest_shapes_of(e)
            .expect("one recorded shape");
        assert_eq!(report.len(), 1, "one shape");
        report[0].rotation.map(f32::to_bits)
    }

    #[test]
    fn recorded_shape_rotation_normalizes_in_scalar_order() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        // With an identity root pose the stored rotation is scalar-normalize(ADVERSARIAL); glam's
        // SIMD normalize pairs (x+z)+(y+w) on SSE2 and rounds differently there.
        let got = recorded_rotation(&mut world, Quat::from_array(ADVERSARIAL));
        assert_eq!(
            got,
            lr_normalize(ADVERSARIAL).map(f32::to_bits),
            "the stored hit-shape pose must normalize in the codec's scalar order on every platform"
        );
    }

    #[test]
    fn rot_trans_normalizes_in_scalar_order() {
        // The root's `Transform.rotation` goes through `to_scale_rotation_translation` (whose
        // dot3-based column lengths are left-to-right on both SSE2 and NEON, so its bits already
        // agree cross-platform) and is then normalized by `rot_trans`. Only that last step is
        // arch-specific; `ADVERSARIAL_ROT` survives the decompose with an adversarial
        // pre-normalize value.
        let q = Quat::from_array(ADVERSARIAL_ROT);
        let a = Affine3A::from_scale_rotation_translation(Vec3::ONE, q, Vec3::new(1.0, 2.0, 3.0));
        let (_, r0, t) = a.to_scale_rotation_translation();
        assert_eq!(t, Vec3::new(1.0, 2.0, 3.0));
        let (got, got_t) = rot_trans(&a);
        assert_eq!(got_t, t);
        assert_eq!(
            bits(got),
            lr_normalize(r0.to_array()).map(f32::to_bits),
            "rot_trans must normalize in the codec's scalar order, not glam's SIMD dot4"
        );
    }

    #[test]
    fn blended_pose_rotation_normalizes_in_scalar_order() {
        let q = Quat::from_array(ADVERSARIAL);
        let a = geom::PosedShape {
            kind: HitShapeKind::Sphere { radius: 1.0 },
            position: Vec3::ZERO,
            rotation: q,
        };
        let b = geom::PosedShape {
            kind: HitShapeKind::Sphere { radius: 1.0 },
            position: Vec3::new(1.0, 0.0, 0.0),
            rotation: Quat::from_array(lr_normalize(ADVERSARIAL)),
        };
        // t = 0: the linear combination is per-component exact (multiply by 1 and 0, add signed
        // zeros), so the blend is exactly `q` and its normalization separates the two orders.
        let at_zero = geom::blend(&a, &b, 0.0);
        assert_eq!(
            bits(at_zero.rotation),
            lr_normalize(ADVERSARIAL).map(f32::to_bits),
            "a fractional rewind's blended rotation must normalize in scalar order"
        );
        // Mid-tick: the blend's components are plain per-lane arithmetic (exact on every
        // platform); pin the normalized result against the same left-to-right reference.
        let t = 0.3f32;
        let other = lr_normalize(ADVERSARIAL);
        let qx = q.x * (1.0 - t) + other[0] * t;
        let qy = q.y * (1.0 - t) + other[1] * t;
        let qz = q.z * (1.0 - t) + other[2] * t;
        let qw = q.w * (1.0 - t) + other[3] * t;
        let mid = geom::blend(&a, &b, t);
        assert_eq!(
            bits(mid.rotation),
            lr_normalize([qx, qy, qz, qw]).map(f32::to_bits)
        );
        // The blend still interpolates position independently of the rounding question.
        assert_eq!(at_zero.position, Vec3::ZERO);
        assert_eq!(mid.position, Vec3::new(0.3, 0.0, 0.0));
    }

    /// Two offset rotations whose recorded poses' dot straddles `SWING_DOT`: left-to-right the
    /// dot is `0x3f6c8365` (< `SWING_DOT`, the shape turned), glam's SSE2 dot4 pairs it to
    /// `0x3f6c8366` (>= `SWING_DOT`, not turned). Both are fixed points of the record path's
    /// `normalize4`, so the recorded bits are exactly these constants (found by search).
    const SWING_R1: [f32; 4] = [0.737_888_93, -0.216_773_73, -0.039_811_71, -0.637_921_7];
    const SWING_R2: [f32; 4] = [0.927_080_5, -0.192_759_19, -0.105_094_165, -0.303_843_5];

    #[test]
    fn swing_uses_scalar_dot_order() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        let mut shape = HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body");
        shape.offset.rotation = Quat::from_array(SWING_R1);
        let e = world
            .spawn((
                NetId(1),
                Transform::IDENTITY,
                HitShapes {
                    shapes: vec![shape],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        world.get_mut::<HitShapes>(e).expect("hit shapes").shapes[0]
            .offset
            .rotation = Quat::from_array(SWING_R2);
        world.resource_mut::<NetNode>().tick = NetTick(2);
        lagcomp_record(&mut world);
        let h = world.resource::<LagCompHistory>();
        let tick2 = h.ticks.iter().find(|r| r.tick == 2).expect("tick 2 recorded");
        let entry = tick2.entries.iter().find(|en| en.entity == e).expect("the entry");
        assert_eq!(
            entry.swing, 0.5,
            "left-to-right the shape turned past SWING_DOT, so the broadphase bounds grow"
        );
    }

    /// Two unit quaternions whose dot straddles zero: left-to-right it is `+0.0` (no sign
    /// flip), glam's SSE2 dot4 gives `-1.49e-8` (flip). `blend` must choose the scalar-order
    /// sign on every platform (found by search).
    const SIGN_A: [f32; 4] = [0.467_784_14, 0.568_748_3, -0.612_140_95, 0.288_074_52];
    const SIGN_B: [f32; 4] = [0.368_422_93, 0.459_121_76, 0.369_843_42, -0.718_809_96];

    #[test]
    fn blend_sign_flip_uses_scalar_dot_order() {
        let a = geom::PosedShape {
            kind: HitShapeKind::Sphere { radius: 1.0 },
            position: Vec3::ZERO,
            rotation: Quat::from_array(SIGN_A),
        };
        let b = geom::PosedShape {
            kind: HitShapeKind::Sphere { radius: 1.0 },
            position: Vec3::new(1.0, 0.0, 0.0),
            rotation: Quat::from_array(SIGN_B),
        };
        // The scalar dot is +0.0 (not negative), so `blend` keeps `b`'s sign: the blend is
        // `a * 0.5 + b * 0.5` per component. Glam's SSE2 dot4 says negative and would flip `b`,
        // producing a different quaternion on x86_64 alone.
        let t = 0.5f32;
        let q = [
            SIGN_A[0] * (1.0 - t) + SIGN_B[0] * t,
            SIGN_A[1] * (1.0 - t) + SIGN_B[1] * t,
            SIGN_A[2] * (1.0 - t) + SIGN_B[2] * t,
            SIGN_A[3] * (1.0 - t) + SIGN_B[3] * t,
        ];
        let blended = geom::blend(&a, &b, t);
        assert_eq!(
            bits(blended.rotation),
            lr_normalize(q).map(f32::to_bits),
            "the short-arc sign flip must follow the scalar dot order on every platform"
        );
    }

    /// A recorded animation-state entry (`PoseSource::Joints` with a capture source): the root
    /// rotation stored in `AnimEntry` must normalize in scalar order, exactly like `rot_trans`.
    #[test]
    fn anim_entry_rotation_normalizes_in_scalar_order() {
        #[derive(Debug)]
        struct Still;
        impl RecordedAnim for Still {
            fn joint_poses(
                &self,
                _root: &Affine3A,
                _joints: &[&str],
                out: &mut Vec<Option<Affine3A>>,
            ) -> bool {
                out.clear();
                true
            }
            fn has_joint(&self, _joint: &str) -> bool {
                false
            }
            fn reach(&self) -> f32 {
                0.0
            }
            fn heap_bytes(&self) -> usize {
                0
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        fn capture(
            _world: &World,
            _e: Entity,
            _seconds: f32,
            _previous: Option<&Arc<dyn RecordedAnim>>,
        ) -> Result<Arc<dyn RecordedAnim>, &'static str> {
            Ok(Arc::new(Still))
        }
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        world.resource_mut::<LagCompAnimSource>().0 = Some(capture);
        let e = world
            .spawn((
                NetId(1),
                Transform::from_rotation(Quat::from_array(ADVERSARIAL_ROT)),
                HitShapes {
                    shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body")],
                    pose_source: PoseSource::Joints,
                },
            ))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        let h = world.resource::<LagCompHistory>();
        let tick1 = h.ticks.iter().find(|r| r.tick == 1).expect("tick 1 recorded");
        let entry = tick1.entries.iter().find(|en| en.entity == e).expect("the entry");
        let anim = entry.anim.as_ref().expect("recorded as animation state");
        // The root rotation goes through `to_scale_rotation_translation` first; `ADVERSARIAL_ROT`
        // survives that decompose with an adversarial pre-normalize value (see the `rot_trans`
        // test), so the reference is the scalar normalization of the decomposed rotation, and
        // the pin fails if this site reverts to glam's SIMD normalize.
        let ent = world.get_entity(e).expect("the entity");
        let (_, r0, _) = root_pose(&world, &ent).to_scale_rotation_translation();
        assert_eq!(
            bits(anim.rotation),
            lr_normalize(r0.to_array()).map(f32::to_bits),
            "the recorded anim root rotation must normalize in the codec's scalar order"
        );
    }

    /// Posing shapes from a recorded animation state (`AnimEntry::pose`, the rewound/query
    /// path): a root-attached shape's rotation is the scalar normalization of its offset
    /// rotation, pinned so a revert to glam's SIMD normalize fails on every platform.
    #[test]
    fn anim_shape_pose_normalizes_in_scalar_order() {
        #[derive(Debug)]
        struct Still;
        impl RecordedAnim for Still {
            fn joint_poses(
                &self,
                _root: &Affine3A,
                _joints: &[&str],
                out: &mut Vec<Option<Affine3A>>,
            ) -> bool {
                out.clear();
                true
            }
            fn has_joint(&self, _joint: &str) -> bool {
                false
            }
            fn reach(&self) -> f32 {
                0.0
            }
            fn heap_bytes(&self) -> usize {
                0
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        fn capture(
            _world: &World,
            _e: Entity,
            _seconds: f32,
            _previous: Option<&Arc<dyn RecordedAnim>>,
        ) -> Result<Arc<dyn RecordedAnim>, &'static str> {
            Ok(Arc::new(Still))
        }
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        world.resource_mut::<LagCompAnimSource>().0 = Some(capture);
        let mut shape = HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body");
        shape.offset.rotation = Quat::from_array(ADVERSARIAL);
        let e = world
            .spawn((
                NetId(1),
                Transform::IDENTITY,
                HitShapes {
                    shapes: vec![shape],
                    pose_source: PoseSource::Joints,
                },
            ))
            .id();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(&mut world);
        let h = world.resource::<LagCompHistory>();
        let tick1 = h.ticks.iter().find(|r| r.tick == 1).expect("tick 1 recorded");
        let entry = tick1.entries.iter().find(|en| en.entity == e).expect("the entry");
        let anim = entry.anim.as_ref().expect("recorded as animation state");
        // An identity root keeps the shape's offset rotation verbatim (a product with the
        // identity is per-component exact), so the posed rotation is scalar-normalize(ADVERSARIAL).
        let mut posed = Vec::new();
        assert!(anim.pose(&mut posed), "the recorded state poses");
        assert_eq!(posed.len(), 1, "one shape");
        assert_eq!(
            bits(posed[0].rotation),
            lr_normalize(ADVERSARIAL).map(f32::to_bits),
            "a shape posed from recorded anim state must normalize in the codec's scalar order"
        );
    }
}
