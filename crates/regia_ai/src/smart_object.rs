// SPDX-License-Identifier: MPL-2.0
//! Smart objects (`docs/specs/31-smart-objects.md`): world entities that advertise interaction
//! slots to AI agents, so an agent can discover, claim and use a chair, terminal, door or cover
//! point without bespoke per-object code.
//!
//! # How the pieces fit
//!
//! An object entity carries [`SmartObject`] (a marker with free-form `tags` and an `enabled`
//! flag). Each *slot* is a child entity with a [`SmartObjectSlot`] (a `role`, its own `enabled`
//! flag and a transient `claimed_by`). The slot's [`regia_scene::Transform`] places and rotates
//! the interaction point in the editor; its world pose is the [`regia_scene::GlobalTransform`].
//!
//! [`SmartObjectPlugin`] rebuilds a [`SmartObjectIndex`] once per frame (in `PostUpdate`, after
//! transform propagation) that maps every slot to its world position and facing, and refreshes it
//! on every fixed step (before `AiStep`) from the local transforms, so a claim a behavior tree
//! makes on the second fixed step of a frame still sees that step's pose, not a frame-old
//! snapshot. An agent queries the index ([`SmartObjectIndex::nearest_available`],
//! [`SmartObjectIndex::available`]) and then claims a slot with [`claim_slot`] / [`release_slot`].
//! Claims are ordinary component state, so scripts and behavior-tree leaf tasks can read them
//! through the world.
//!
//! The index is a linear list: it is rebuilt every frame and each query scans it. That is correct
//! and cheap for the hundreds of slots a level holds; a spatial structure is a follow-up (see the
//! spec's acceptance section). A slot is *available* when it is enabled, its owning object is
//! enabled, and it is not claimed.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_asset::{AssetRef, SourceAssetChanged};
use regia_core::math::Vec3;
use regia_ecs::ecs::system::SystemParam;
use regia_framework::tags::{TagContainer, TagQuery};
#[cfg(feature = "debug_draw")]
use regia_render::gizmos::{Gizmos, SmartObjectGizmos};
use regia_scene::layers::LayerParked;
use regia_scene::{GlobalTransform, Transform};

use crate::smart_object_def::SmartObjectDef;
use crate::smart_object_v2::{ClaimHandle, ClaimPriority};

/// A world object that advertises interaction affordances through its child [`SmartObjectSlot`]s.
///
/// The object itself is an ordinary entity (usually carrying a `Transform` and meshes); its slots
/// are child entities placed and rotated in the editor. A slot with no [`SmartObject`] ancestor is
/// still a valid slot, but it cannot be disabled through its object.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct SmartObject {
    /// Free-form tags classifying what the object affords as a whole (e.g. `"cover"`, `"furniture"`).
    pub tags: Vec<String>,
    /// When `false`, every slot of this object is undiscoverable and unclaimable.
    pub enabled: bool,
    /// Typed tags added and removed at run time (`add_runtime_tag`); they join the activity set of
    /// every slot of the object. Transient: never persisted (spec 39 § 4).
    #[reflect(ignore)]
    pub runtime_tags: TagContainer,
    /// Reasons the object is switched off (`set_enabled`); the object is enabled only when
    /// `enabled` is true and this set is empty. Transient.
    #[reflect(ignore)]
    pub off_reasons: BTreeSet<String>,
    /// The `.smartobject.ron` this object takes its slots from (persisted, spec 39 § 4): read
    /// through the asset server, see [`crate::smart_object_load`].
    pub definition: Option<AssetRef<SmartObjectDef>>,
}

impl Default for SmartObject {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            enabled: true,
            runtime_tags: TagContainer::new(),
            off_reasons: BTreeSet::new(),
            definition: None,
        }
    }
}

/// One interaction point of a [`SmartObject`]: a place an agent can stand (or sit) to perform the
/// interaction, keyed by its `role`.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct SmartObjectSlot {
    /// The interaction role this slot affords (e.g. `"sit"`, `"operate"`, `"open"`, `"cover"`).
    pub role: String,
    /// When `false`, this slot is undiscoverable and unclaimable.
    pub enabled: bool,
    /// The agent (or player entity) that currently holds the claim, if any. Transient runtime
    /// state: never persisted or reflected (`#[reflect(ignore)]`).
    #[reflect(ignore)]
    pub claimed_by: Option<Entity>,
    /// The slot's index in its object's definition, set on slots materialized from a definition.
    pub def_slot: Option<u16>,
    /// Activity tags this slot offers (for hand-built slots; spec 39 § 5.2).
    pub activity: TagContainer,
    /// The slot's own requester rule (spec 39 § 5.3). Not reflected (`TagQuery` has no reflection).
    #[reflect(ignore)]
    pub user_query: Option<TagQuery>,
    /// The priority of the current claim. Transient.
    #[reflect(ignore)]
    pub priority_held: ClaimPriority,
    /// Whether the holder has begun using the slot. Transient.
    #[reflect(ignore)]
    pub in_use: bool,
    /// Bumped whenever the slot is released or invalidated, so an old [`ClaimHandle`] never matches.
    #[reflect(ignore)]
    pub generation: u32,
    /// Typed tags added and removed at run time. Transient.
    #[reflect(ignore)]
    pub runtime_tags: TagContainer,
    /// Reasons the slot is switched off (`set_enabled`). Transient.
    #[reflect(ignore)]
    pub off_reasons: BTreeSet<String>,
}

impl Default for SmartObjectSlot {
    fn default() -> Self {
        Self {
            role: String::new(),
            enabled: true,
            claimed_by: None,
            def_slot: None,
            activity: TagContainer::new(),
            user_query: None,
            priority_held: ClaimPriority::Normal,
            in_use: false,
            generation: 0,
            runtime_tags: TagContainer::new(),
            off_reasons: BTreeSet::new(),
        }
    }
}

impl SmartObjectSlot {
    /// A slot for `role`, enabled and unclaimed.
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            ..Self::default()
        }
    }

    /// The handle of the current claim, if the slot is claimed.
    pub fn handle(&self, slot: Entity) -> Option<ClaimHandle> {
        self.claimed_by.map(|_| ClaimHandle {
            slot,
            generation: self.generation,
        })
    }
}

/// The error returned by [`claim_slot`] and [`release_slot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SmartObjectError {
    /// No entity with a [`SmartObjectSlot`] at `0`.
    #[error("`{0}` is not a smart-object slot")]
    SlotNotFound(Entity),
    /// The slot (or its owning object) is disabled.
    #[error("smart-object slot `{0}` is disabled")]
    SlotDisabled(Entity),
    /// Another agent holds the claim.
    #[error("smart-object slot `{0}` is already claimed by `{1}`")]
    SlotClaimed(Entity, Entity),
    /// `release_slot` was called by an agent that does not hold the claim.
    #[error("smart-object slot `{0}` is not claimed by `{1}`")]
    NotClaimedBy(Entity, Entity),
    /// A [`ClaimHandle`] whose claim was released, preempted or invalidated.
    #[error("the claim handle is stale")]
    StaleHandle,
    /// The slot is not claimed by the user named.
    #[error("the user does not hold the claim")]
    NotHolder,
    /// The slot is in use and cannot be claimed or preempted.
    #[error("smart-object slot `{0}` is in use")]
    SlotInUse(Entity),
}

/// One indexed slot, borrowed from the [`SmartObjectIndex`]. `available` is shorthand for
/// `enabled && !claimed`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotInfo<'a> {
    /// The slot entity (the entity carrying the [`SmartObjectSlot`]).
    pub entity: Entity,
    /// The nearest [`SmartObject`] ancestor, or [`Entity::PLACEHOLDER`] when the slot has none.
    pub object: Entity,
    /// The slot's role (borrowed from the index).
    pub role: &'a str,
    /// The slot's world position.
    pub position: Vec3,
    /// The slot's world-space facing direction (its local −Z axis).
    pub forward: Vec3,
    /// Whether the slot and its owning object are enabled.
    pub enabled: bool,
    /// Whether an agent currently holds the claim.
    pub claimed: bool,
}

impl SlotInfo<'_> {
    /// Whether the slot can be claimed right now: enabled and unclaimed.
    pub fn available(&self) -> bool {
        self.enabled && !self.claimed
    }
}

/// The owned form of one indexed slot, stored in the [`SmartObjectIndex`].
#[derive(Debug, Clone)]
struct SlotEntry {
    entity: Entity,
    object: Entity,
    role: String,
    position: Vec3,
    forward: Vec3,
    enabled: bool,
    claimed: bool,
    /// The slot's arrival key, computed once per rebuild: the index sorts by (role, arrival).
    arrival: regia_ecs::spawn_order::SpawnKey,
}

impl SlotEntry {
    fn info(&self) -> SlotInfo<'_> {
        SlotInfo {
            entity: self.entity,
            object: self.object,
            role: &self.role,
            position: self.position,
            forward: self.forward,
            enabled: self.enabled,
            claimed: self.claimed,
        }
    }
}

/// An index of every [`SmartObjectSlot`], kept by [`SmartObjectPlugin`] after transform propagation
/// and rebuilt only on a frame where something it reads changed. Not reflected: it is derived data,
/// not authored state.
///
/// Queries are linear scans of the slot list. The index is ordered by role for a deterministic
/// [`SmartObjectIndex::for_role`] / [`SmartObjectIndex::available`] result (ties break by entity
/// bits), so two agents querying the same role agree on a stable order.
#[derive(Resource)]
pub struct SmartObjectIndex {
    slots: Vec<SlotEntry>,
    /// Edge length of a grid cell in metres (spec 39 § 9). The grid exists only above
    /// [`GRID_MIN_SLOTS`] slots.
    pub cell_size: f32,
    grid: Option<(f32, SlotGrid)>,
    rebuilds: u64,
}

/// The region grid: cell coordinates to indices into the slot list.
type SlotGrid = BTreeMap<[i32; 3], Vec<u32>>;

/// Above this many slots the index keeps a uniform grid for region queries (spec 39 § 9).
pub const GRID_MIN_SLOTS: usize = 256;

/// The default grid cell edge in metres.
pub const GRID_CELL_SIZE: f32 = 8.0;

impl Default for SmartObjectIndex {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            cell_size: GRID_CELL_SIZE,
            grid: None,
            rebuilds: 0,
        }
    }
}

fn cell_of(position: Vec3, cell_size: f32) -> [i32; 3] {
    // `as i32` saturates, so a huge coordinate lands in an edge cell instead of wrapping.
    [
        (position.x / cell_size).floor() as i32,
        (position.y / cell_size).floor() as i32,
        (position.z / cell_size).floor() as i32,
    ]
}

impl SmartObjectIndex {
    /// How many times the index has been rebuilt. A frame where nothing it reads changed adds
    /// nothing (spec 39 § 12).
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// The number of indexed slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether there are no slots.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Every indexed slot, in role order.
    pub fn slots(&self) -> impl Iterator<Item = SlotInfo<'_>> {
        self.slots.iter().map(SlotEntry::info)
    }

    /// The slot with entity `entity`, if it is indexed.
    pub fn slot(&self, entity: Entity) -> Option<SlotInfo<'_>> {
        self.slots
            .iter()
            .find(|s| s.entity == entity)
            .map(SlotEntry::info)
    }

    /// Every indexed slot whose role is `role`, in a stable order.
    pub fn for_role(&self, role: &str) -> Vec<SlotInfo<'_>> {
        self.slots
            .iter()
            .filter(|s| s.role == role)
            .map(SlotEntry::info)
            .collect()
    }

    /// Every currently claimable slot whose role is `role`.
    pub fn available(&self, role: &str) -> Vec<SlotInfo<'_>> {
        self.slots
            .iter()
            .filter(|s| s.role == role && s.enabled && !s.claimed)
            .map(SlotEntry::info)
            .collect()
    }

    /// The nearest claimable slot whose role is `role` to `position`, or `None`.
    ///
    /// A non-finite `position` answers `None`. Distance is the squared 3-D length to the slot's
    /// world position; an exact tie keeps the earlier entry of the index — the earlier arrival of
    /// `role` (see [`SmartObjectIndex`]), never the lower entity id.
    pub fn nearest_available(&self, position: Vec3, role: &str) -> Option<SlotInfo<'_>> {
        if !position.is_finite() {
            return None;
        }
        self.slots
            .iter()
            .filter(|s| s.role == role && s.enabled && !s.claimed)
            .min_by(|a, b| {
                let da = (a.position - position).length_squared();
                let db = (b.position - position).length_squared();
                da.total_cmp(&db)
            })
            .map(SlotEntry::info)
    }

    /// Whether the uniform grid is active (more than [`GRID_MIN_SLOTS`] slots).
    pub fn has_grid(&self) -> bool {
        self.grid.is_some()
    }

    /// The slots a search over `region` has to look at, in index order, and the number of grid
    /// cells visited (0 when the whole list is scanned). The grid answers a superset of the slots
    /// inside the region; without a grid, or when the region would touch more cells than there
    /// are slots, every slot is returned.
    pub fn candidates(&self, region: &crate::smart_object_v2::QueryRegion) -> (Vec<SlotInfo<'_>>, usize) {
        let Some((lo, hi, grid)) = self.grid_span(region) else {
            return (self.slots.iter().map(SlotEntry::info).collect(), 0);
        };
        let mut indices: Vec<u32> = Vec::new();
        let mut visited = 0;
        for_cells(lo, hi, |cell| {
            visited += 1;
            if let Some(found) = grid.get(&cell) {
                indices.extend_from_slice(found);
            }
        });
        indices.sort_unstable();
        let slots = indices
            .into_iter()
            .filter_map(|i| self.slots.get(i as usize))
            .map(SlotEntry::info)
            .collect();
        (slots, visited)
    }

    /// The grid cells a search over `region` visits, when the grid is the way to answer it: none
    /// without a grid, for an invalid region, or when it would touch more cells than there are
    /// slots.
    fn grid_span(&self, region: &crate::smart_object_v2::QueryRegion) -> Option<GridSpan<'_>> {
        let (Some((built_size, grid)), true) = (&self.grid, region.is_valid()) else {
            return None;
        };
        // A zero, negative or non-finite size reverses or collapses the cell range: scan instead.
        if !(built_size.is_finite() && *built_size > 0.0) {
            return None;
        }
        let (lo, hi) = match *region {
            crate::smart_object_v2::QueryRegion::Sphere { center, radius } => {
                (center - Vec3::splat(radius), center + Vec3::splat(radius))
            }
            crate::smart_object_v2::QueryRegion::Aabb { min, max } => (min, max),
        };
        let (lo, hi) = (cell_of(lo, *built_size), cell_of(hi, *built_size));
        let span = |axis: usize| i64::from(hi[axis]) - i64::from(lo[axis]) + 1;
        let cells = span(0).saturating_mul(span(1)).saturating_mul(span(2));
        (cells <= self.slots.len() as i64).then_some((lo, hi, grid))
    }

    /// Calls `visit` for the slots of [`candidates`](Self::candidates), without building a list:
    /// the path of a search with no log (spec 39 § 5.1). The second argument is the slot's
    /// ordinal in [`slots`](Self::slots) (the role, then arrival, order): an exact distance tie
    /// must resolve to it, never to this visitation order, so a search gives the same result
    /// with or without a log (the grid visits cells in cell-coordinate order, the linear scan in
    /// index order — two different orders for the same slots).
    pub(crate) fn for_each_candidate<'a>(
        &'a self,
        region: &crate::smart_object_v2::QueryRegion,
        mut visit: impl FnMut(SlotInfo<'a>, u32),
    ) {
        let Some((lo, hi, grid)) = self.grid_span(region) else {
            self.slots
                .iter()
                .enumerate()
                .for_each(|(i, slot)| visit(slot.info(), i as u32));
            return;
        };
        for_cells(lo, hi, |cell| {
            for &index in grid.get(&cell).map_or(&[][..], Vec::as_slice) {
                if let Some(slot) = self.slots.get(index as usize) {
                    visit(slot.info(), index);
                }
            }
        });
    }

    /// The nearest [`SmartObject`] ancestor of `entity` (including `entity` itself), or `None`.
    ///
    /// Returns `None` when the entity (or every ancestor) has been despawned.
    pub fn object_of(&self, entity: Entity) -> Option<Entity> {
        self.slot(entity)
            .map(|s| s.object)
            .filter(|o| *o != Entity::PLACEHOLDER)
    }
}

/// The deepest `ChildOf` chain the ancestor walks follow; a longer one (only a parent cycle gets
/// there) counts as having no smart object.
const MAX_PARENT_WALK: usize = 4096;

/// The nearest [`SmartObject`] ancestor of `entity` (including `entity` itself), or `None`. Walks
/// the `ChildOf` links with [`World`] access, so a behavior-tree leaf task or a script can call
/// it directly.
pub fn smart_object_ancestor(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = Some(entity);
    // A `ChildOf` cycle (A under B under A) is not stripped by bevy: bound the walk.
    for _ in 0..MAX_PARENT_WALK {
        let Some(current) = cursor else { break };
        if world.get::<SmartObject>(current).is_some() {
            return Some(current);
        }
        cursor = world.get::<ChildOf>(current).map(|link| link.parent());
    }
    None
}

/// Whether `entity` is a claimable slot right now: it has an enabled [`SmartObjectSlot`] and, when
/// it has a [`SmartObject`] ancestor, that ancestor is enabled too. `entity` without a slot answers
/// `false`.
pub fn effective_enabled(world: &World, entity: Entity) -> bool {
    let Some(slot) = world.get::<SmartObjectSlot>(entity) else {
        return false;
    };
    if !slot.enabled || !slot.off_reasons.is_empty() {
        return false;
    }
    smart_object_ancestor(world, entity)
        .and_then(|object| world.get::<SmartObject>(object))
        .is_none_or(|object| object.enabled && object.off_reasons.is_empty())
}

/// Claims slot `entity` for `agent` at [`ClaimPriority::Normal`]. Idempotent for the same agent; a
/// claim by another agent fails with [`SmartObjectError::SlotClaimed`]. A disabled slot (or disabled
/// object) fails with [`SmartObjectError::SlotDisabled`]. See [`crate::smart_object_v2::claim_with`]
/// for priorities and handles.
pub fn claim_slot(world: &mut World, slot: Entity, agent: Entity) -> Result<(), SmartObjectError> {
    crate::smart_object_v2::claim_with(world, slot, agent, ClaimPriority::Normal).map(|_| ())
}

/// Releases slot `entity` held by `agent`. Releasing an unclaimed slot is a no-op (idempotent);
/// releasing a slot held by another agent fails with [`SmartObjectError::NotClaimedBy`].
pub fn release_slot(world: &mut World, slot: Entity, agent: Entity) -> Result<(), SmartObjectError> {
    let Some(state) = world.get::<SmartObjectSlot>(slot) else {
        return Err(SmartObjectError::SlotNotFound(slot));
    };
    match state.claimed_by {
        Some(holder) if holder == agent => crate::smart_object_v2::release_claim(world, slot),
        Some(holder) => Err(SmartObjectError::NotClaimedBy(slot, holder)),
        None => Ok(()),
    }
}

/// Marker color of an available slot in the debug overlay.
pub const AVAILABLE_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.2, 0.9, 0.4);
/// Marker color of a claimed slot in the debug overlay.
pub const CLAIMED_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.9, 0.6, 0.2);
/// Marker color of a disabled slot in the debug overlay.
pub const DISABLED_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.4, 0.4, 0.4);

/// The fixed-step and overlay systems of the smart-object feature. Adds the [`SmartObjectIndex`]
/// resource and rebuilds it each frame; the debug overlay draws into the `SmartObjectGizmos`
/// group (off by default, `debug.set_gizmos { "group": "smart_object" }`).
#[derive(Default)]
pub struct SmartObjectPlugin;

impl Plugin for SmartObjectPlugin {
    fn build(&self, app: &mut App) {
        // The index order and the query tie-breaks follow the slots' arrival order, never their
        // entity ids (ids are record-time allocations, ADR-1347). Materialization also follows
        // the definitions' arrival order (not entity ids), so the slots two objects spawn in one
        // frame arrive in the order the definitions did.
        regia_ecs::spawn_order::track_spawn_order::<SmartObjectSlot>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<crate::smart_object_def::SmartObjectDefinition>(
            app.world_mut(),
        );
        app.register_type::<SmartObject>()
            .register_type::<SmartObjectSlot>()
            .register_type::<AssetRef<SmartObjectDef>>()
            .init_resource::<SmartObjectIndex>()
            .init_resource::<SmartObjectIndexStale>()
            .init_resource::<SmartObjectIndexStepStale>()
            .init_resource::<crate::smart_object_load::SmartObjectDefs>()
            .add_message::<SourceAssetChanged>()
            .add_observer(stale_on_parent_set)
            .add_observer(stale_on_parent_cleared)
            .add_observer(stale_on_slot_removed)
            .add_observer(stale_on_object_removed)
            .add_observer(stale_on_transform_removed)
            .add_observer(stale_on_slot_parked)
            .add_observer(stale_on_slot_unparked);
        crate::smart_object_v2::build(app);
        app.add_systems(
            PostUpdate,
            (
                crate::smart_object_load::smart_object_load_definitions,
                crate::smart_object_def::smart_object_materialize,
            )
                .chain()
                .before(PostUpdateSet::TransformPropagate),
        )
        .add_systems(
            PostUpdate,
            (
                crate::smart_object_v2::smart_object_auto_release,
                crate::smart_object_v2::flush_smart_object_events,
                crate::smart_object_v2::sweep_behavior_cache,
                rebuild_smart_object_index,
            )
                .chain()
                .after(PostUpdateSet::TransformPropagate),
        )
        .add_systems(
            FixedUpdate,
            rebuild_smart_object_index_step
                .after(regia_physics::PhysicsStep)
                .before(crate::AiStep),
        );
        #[cfg(feature = "debug_draw")]
        app.add_systems(
            PostUpdate,
            draw_smart_object_slots
                .run_if(regia_render::gizmos::gizmo_group_enabled::<SmartObjectGizmos>)
                .after(rebuild_smart_object_index),
        );
    }
}

/// The first and last cell of a grid search and the grid it reads.
type GridSpan<'a> = ([i32; 3], [i32; 3], &'a BTreeMap<[i32; 3], Vec<u32>>);

fn for_cells(lo: [i32; 3], hi: [i32; 3], mut visit: impl FnMut([i32; 3])) {
    for x in lo[0]..=hi[0] {
        for y in lo[1]..=hi[1] {
            for z in lo[2]..=hi[2] {
                visit([x, y, z]);
            }
        }
    }
}

/// Set by the observers below for the changes the index cannot filter for: a removal, and a parent
/// link anywhere above a slot (a `Changed<ChildOf>` filter would walk every child in the world each
/// frame to find out nothing moved). Each schedule that rebuilds (the frame snapshot in
/// `PostUpdate`, the fixed-step refresh) takes its own flag, so one rebuild consuming the flag does
/// not hide the change from the other.
#[derive(Resource, Default)]
struct SmartObjectIndexStale(bool);

/// The fixed-step counterpart of [`SmartObjectIndexStale`].
#[derive(Resource, Default)]
struct SmartObjectIndexStepStale(bool);

fn set_index_stale(stale: &mut SmartObjectIndexStale, step_stale: &mut SmartObjectIndexStepStale) {
    stale.0 = true;
    step_stale.0 = true;
}

fn stale_on_parent_set(
    _: On<Insert<ChildOf>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    set_index_stale(&mut stale, &mut step_stale);
}

fn stale_on_parent_cleared(
    _: On<Remove<ChildOf>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    set_index_stale(&mut stale, &mut step_stale);
}

fn stale_on_slot_removed(
    _: On<Remove<SmartObjectSlot>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    set_index_stale(&mut stale, &mut step_stale);
}

fn stale_on_object_removed(
    _: On<Remove<SmartObject>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    set_index_stale(&mut stale, &mut step_stale);
}

fn stale_on_transform_removed(
    on: On<Remove<GlobalTransform>>,
    slots: Query<(), With<SmartObjectSlot>>,
    parents: Query<(), With<Children>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    // The rebuild chain-reads an ancestor's `GlobalTransform` whenever the ancestor has no local
    // `Transform` of its own (deep-zai lower note): an ancestor losing its world transform moves
    // every slot under it exactly the way the slot losing its own does. Only a slot, or an
    // entity with children (a possible ancestor), can change a chain-read pose; a childless
    // non-slot entity's removal stays quiet.
    let entity = on.event().entity;
    if slots.contains(entity) || parents.contains(entity) {
        set_index_stale(&mut stale, &mut step_stale);
    }
}

/// Parking a layer only inserts [`LayerParked`] on the slot (and its object): no slot, object
/// or transform change for the index to see, and the rebuild leaves parked slots out (ADR-2396).
fn stale_on_slot_parked(
    on: On<Insert<LayerParked>>,
    slots: Query<(), With<SmartObjectSlot>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    if slots.contains(on.event().entity) {
        set_index_stale(&mut stale, &mut step_stale);
    }
}

/// Unparking removes [`LayerParked`]: the slot comes back into the index.
fn stale_on_slot_unparked(
    on: On<Remove<LayerParked>>,
    slots: Query<(), With<SmartObjectSlot>>,
    mut stale: ResMut<SmartObjectIndexStale>,
    mut step_stale: ResMut<SmartObjectIndexStepStale>,
) {
    if slots.contains(on.event().entity) {
        set_index_stale(&mut stale, &mut step_stale);
    }
}

/// A slot whose component or world transform changed.
type ChangedSlot = (
    With<SmartObjectSlot>,
    Or<(Changed<SmartObjectSlot>, Changed<GlobalTransform>)>,
);

/// What can move a slot's pose between fixed steps: a local `Transform` of anything at all (a
/// parent move reaches every child slot without touching it), or the `GlobalTransform` of a
/// `GlobalTransform`-only slot the rebuild falls back to.
type StepMoved = Or<(Changed<Transform>, Changed<GlobalTransform>)>;

/// Everything that can make the [`SmartObjectIndex`] out of date.
#[derive(SystemParam)]
struct IndexWatch<'w, 's> {
    stale: ResMut<'w, SmartObjectIndexStale>,
    slots: Query<'w, 's, (), ChangedSlot>,
    objects: Query<'w, 's, (), Changed<SmartObject>>,
}

/// The fixed-step counterpart of [`IndexWatch`]: what a claim on this step can see change. A
/// local `Transform` move of anything at all counts — physics moves parents, not slots, and a
/// parent move changes every child slot's pose without touching the slot itself. So does a
/// `GlobalTransform`-only slot's own move: the rebuild falls back to `GlobalTransform` for a
/// slot that has no local `Transform`, so watching only `Transform` would strand the index on
/// the old pose until `PostUpdate`. Static worlds cost one change-tick archetype scan per step
/// and rebuild nothing.
#[derive(SystemParam)]
struct StepIndexWatch<'w, 's> {
    stale: ResMut<'w, SmartObjectIndexStepStale>,
    moved: Query<'w, 's, (), StepMoved>,
    slots: Query<'w, 's, (), Changed<SmartObjectSlot>>,
    objects: Query<'w, 's, (), Changed<SmartObject>>,
}

impl IndexWatch<'_, '_> {
    /// Whether anything changed since the last call.
    fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.stale.0) || !self.slots.is_empty() || !self.objects.is_empty()
    }
}

impl StepIndexWatch<'_, '_> {
    /// Whether anything changed since the last call.
    fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.stale.0)
            || !self.moved.is_empty()
            || !self.slots.is_empty()
            || !self.objects.is_empty()
    }
}

/// The slot rows the index is built from. `GlobalTransform` is required (a slot is placed in the
/// world); the local `Transform` is read when present.
type SlotRows<'w, 's> =
    Query<'w, 's, (Entity, &'static SmartObjectSlot), (With<GlobalTransform>, Without<LayerParked>)>;

/// The pose lookup both rebuilds share: local `Transform` (falling back to `GlobalTransform`) and
/// the `ChildOf` link, for the slot and every ancestor.
type SlotPoses<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static Transform>,
        Option<&'static GlobalTransform>,
        Option<&'static ChildOf>,
    ),
>;

/// Rebuilds the index from the slot rows. Positions come from the local-transform chain (the
/// `GlobalTransform` snapshot would be a frame old on a later fixed step of a frame); the sort and
/// every distance tie-break follow the slots' arrival order, never their entity ids (ADR-1347).
#[allow(clippy::too_many_arguments)]
fn rebuild_index_core(
    index: &mut SmartObjectIndex,
    slots: &SlotRows<'_, '_>,
    objects: &Query<&SmartObject>,
    poses: &SlotPoses<'_, '_>,
    order: Option<&regia_ecs::spawn_order::SpawnOrder<SmartObjectSlot>>,
    rebuilt: &mut Vec<SlotEntry>,
) {
    index.rebuilds += 1;
    let key = |entity: Entity| {
        order.map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
            o.key(entity)
        })
    };
    // Last frame's list, swapped out below, is the buffer of this one: no per-frame allocation
    // once every role fits (spec 39 § 12).
    let mut written = 0;
    for (entity, slot) in slots.iter() {
        // Resolve the nearest SmartObject ancestor (or none), and its enabled flag. The walk
        // starts at the slot itself, as `smart_object_ancestor` (and so `claim_slot`) does: a
        // `SmartObject` on the slot entity counts (ADR-1809).
        let mut object: Option<(Entity, bool)> = None;
        let mut cursor = Some(entity);
        for _ in 0..MAX_PARENT_WALK {
            let Some(at) = cursor else { break };
            if let Ok(object_component) = objects.get(at) {
                object = Some((
                    at,
                    object_component.enabled && object_component.off_reasons.is_empty(),
                ));
                break;
            }
            cursor = poses
                .get(at)
                .ok()
                .and_then(|(_, _, link)| link.map(ChildOf::parent));
        }
        let (object_entity, object_enabled) = object.unwrap_or((Entity::PLACEHOLDER, true));
        let (position, forward) = {
            let (transform, global, link) = poses.get(entity).unwrap_or((None, None, None));
            crate::tick_budget::pose_from_chain(transform, global, link, |e| {
                poses.get(e).unwrap_or((None, None, None))
            })
        }
        .unwrap_or((Vec3::ZERO, Vec3::NEG_Z));
        let enabled = slot.enabled && slot.off_reasons.is_empty() && object_enabled;
        let claimed = slot.claimed_by.is_some();
        match rebuilt.get_mut(written) {
            Some(entry) => {
                entry.entity = entity;
                entry.object = object_entity;
                entry.role.clone_from(&slot.role);
                entry.position = position;
                entry.forward = forward;
                entry.enabled = enabled;
                entry.claimed = claimed;
                entry.arrival = key(entity);
            }
            None => rebuilt.push(SlotEntry {
                entity,
                object: object_entity,
                role: slot.role.clone(),
                position,
                forward,
                enabled,
                claimed,
                arrival: key(entity),
            }),
        }
        written += 1;
    }
    rebuilt.truncate(written);
    // Stable order: role, then the slot's arrival. Queries agree with each other and with the
    // editor, and two runs of the same level that allocated different ids still agree (the
    // spawn-order key falls back to the entity id only for a slot the tracking never saw).
    // The arrival key is cached on each entry while the list is filled (a hash-map lookup per
    // comparison showed up at thousand-slot scale). One unstable sort on (role, arrival): the
    // keys are unique per slot, so the order is total, and an unstable sort allocates nothing
    // (a stable `sort_by` takes a heap scratch buffer once the list outgrows its stack buffer,
    // one allocation per rebuild: `tests/smart_object_alloc.rs`).
    rebuilt.sort_unstable_by(|a, b| a.role.cmp(&b.role).then(a.arrival.cmp(&b.arrival)));
    // The grid depends only on which slots exist, their order and their positions, so it is kept
    // while those are unchanged (claims, flags and roles-with-the-same-order do not touch it).
    let same_entities = index.slots.len() == rebuilt.len()
        && index
            .slots
            .iter()
            .zip(rebuilt.iter())
            .all(|(old, new)| old.entity == new.entity);
    let same_layout = same_entities
        && index
            .slots
            .iter()
            .zip(rebuilt.iter())
            .all(|(old, new)| old.position == new.position);
    // After the swap, `rebuilt` is the previous list and `index.slots` is the new one.
    std::mem::swap(&mut index.slots, &mut *rebuilt);
    if index.slots.len() <= GRID_MIN_SLOTS {
        index.grid = None;
    } else if same_layout
        && index
            .grid
            .as_ref()
            .is_some_and(|(size, _)| *size == index.cell_size)
    {
        // Positions and cell membership are unchanged.
    } else if same_entities
        && index
            .grid
            .as_ref()
            .is_some_and(|(size, _)| *size == index.cell_size)
    {
        // Same slots in the same order: move an index only when its cell changes. Empty cell
        // lists stay, so a slot that comes back does not allocate a new vector.
        let cell_size = index.cell_size;
        let (slots, grid) = (&index.slots, &mut index.grid);
        let Some((_, grid)) = grid.as_mut() else {
            return;
        };
        for (i, (old, new)) in rebuilt.iter().zip(slots.iter()).enumerate() {
            let old_cell = old.position.is_finite().then(|| cell_of(old.position, cell_size));
            let new_cell = new.position.is_finite().then(|| cell_of(new.position, cell_size));
            if old_cell == new_cell {
                continue;
            }
            let slot_index = i as u32;
            if let Some(cell) = old_cell
                && let Some(list) = grid.get_mut(&cell)
                && let Some(at) = list.iter().position(|&slot| slot == slot_index)
            {
                list.swap_remove(at);
            }
            if let Some(cell) = new_cell {
                grid.entry(cell).or_default().push(slot_index);
            }
        }
    } else {
        let mut cells: BTreeMap<[i32; 3], Vec<u32>> = BTreeMap::new();
        for (i, slot) in index.slots.iter().enumerate() {
            if slot.position.is_finite() {
                cells
                    .entry(cell_of(slot.position, index.cell_size))
                    .or_default()
                    .push(i as u32);
            }
        }
        index.grid = Some((index.cell_size, cells));
    }
}

/// `PostUpdate` (after `TransformPropagate`): rebuilds the [`SmartObjectIndex`] when a slot, a
/// world transform or an object changed, so a frame holds a consistent snapshot for the frame's
/// queries and the editor overlay. An unchanged frame costs one change-tick walk over the slots
/// and the objects (spec 39 § 12); a rebuild is a linear walk over the slots plus one `ChildOf` hop
/// per slot.
#[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_index")]
fn rebuild_smart_object_index(
    mut index: ResMut<SmartObjectIndex>,
    mut watch: IndexWatch,
    slots: SlotRows<'_, '_>,
    objects: Query<&SmartObject>,
    poses: SlotPoses<'_, '_>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<SmartObjectSlot>>>,
    mut rebuilt: Local<Vec<SlotEntry>>,
) {
    let _span = tracing::debug_span!("rebuild_smart_object_index").entered();
    // `index.is_changed()` is a host's write (`cell_size`, a replaced resource); this system's
    // own writes do not count on its next run, and the fixed-step refresh bypasses change
    // detection so its rebuilds do not count here either.
    if !watch.take_changed() && !index.is_changed() {
        return;
    }
    rebuild_index_core(
        &mut index,
        &slots,
        &objects,
        &poses,
        order.as_deref(),
        &mut rebuilt,
    );
}

/// `FixedUpdate` (after `PhysicsStep`, before `AiStep`): refreshes the index the simulation claims
/// read, from this step's local transforms. The `PostUpdate` snapshot is a frame old on every
/// fixed step but the first of a frame, so two runs at different frame rates would claim different
/// slots at the same step (spec 39 § 12).
#[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_index_step")]
fn rebuild_smart_object_index_step(
    mut index: ResMut<SmartObjectIndex>,
    mut watch: StepIndexWatch,
    slots: SlotRows<'_, '_>,
    objects: Query<&SmartObject>,
    poses: SlotPoses<'_, '_>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<SmartObjectSlot>>>,
    mut rebuilt: Local<Vec<SlotEntry>>,
) {
    let _span = tracing::debug_span!("rebuild_smart_object_index_step").entered();
    if !watch.take_changed() {
        return;
    }
    // Write without marking the resource changed: a system sees another system's writes, so the
    // `PostUpdate` snapshot system would take this write for a host's and rebuild the identical
    // data again on every frame that had a fixed-step refresh. A host write (the editor's
    // `cell_size`, a replaced resource) still sets the change tick and is honoured there.
    rebuild_index_core(
        index.bypass_change_detection(),
        &slots,
        &objects,
        &poses,
        order.as_deref(),
        &mut rebuilt,
    );
}

/// The overlay color of a claim held at `priority`: cool for background work, warm for urgent.
pub fn priority_color(priority: ClaimPriority) -> regia_core::math::Color {
    use regia_core::math::Color;
    match priority {
        ClaimPriority::Background => Color::rgb(0.45, 0.55, 0.75),
        ClaimPriority::Low => Color::rgb(0.3, 0.6, 0.9),
        ClaimPriority::Normal => CLAIMED_COLOR,
        ClaimPriority::High => Color::rgb(0.95, 0.4, 0.15),
        ClaimPriority::Urgent => Color::rgb(0.95, 0.15, 0.15),
    }
}

/// Points of a horizontal circle (the XZ plane) around `center`.
#[cfg(feature = "debug_draw")]
fn ring_points(center: Vec3, radius: f32) -> impl Iterator<Item = Vec3> {
    const SEGMENTS: u32 = 16;
    (0..SEGMENTS).map(move |i| {
        let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        center + Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
    })
}

/// `PostUpdate` debug overlay (feature `debug_draw`): a wire sphere and a facing ray per slot, colored by state
/// (available / claimed / disabled); a claimed slot also gets a ring colored by the holder's
/// priority (two rings and spokes once it is in use), and every entrance an arrow pointing at its
/// slot. Runs only while the `SmartObjectGizmos` group is enabled.
#[cfg(feature = "debug_draw")]
pub fn draw_smart_object_slots(
    index: Res<SmartObjectIndex>,
    states: Query<&SmartObjectSlot>,
    entrances: Query<(&crate::smart_object_v2::SlotEntrance, &GlobalTransform, &ChildOf)>,
    mut gizmos: Gizmos<SmartObjectGizmos>,
) {
    let _span = tracing::debug_span!("draw_smart_object_slots").entered();
    for slot in index.slots() {
        let color = if !slot.enabled {
            DISABLED_COLOR
        } else if slot.claimed {
            CLAIMED_COLOR
        } else {
            AVAILABLE_COLOR
        };
        gizmos.sphere(slot.position, 0.08, color);
        gizmos.ray(slot.position, slot.forward * 0.4, color);
        if let (true, Ok(state)) = (slot.claimed, states.get(slot.entity)) {
            let ring = priority_color(state.priority_held);
            gizmos.lineloop(ring_points(slot.position, 0.25), ring);
            if state.in_use {
                gizmos.lineloop(ring_points(slot.position, 0.15), ring);
                for point in ring_points(slot.position, 0.25).step_by(4) {
                    gizmos.line(slot.position, point, ring);
                }
            }
        }
    }
    for (_, transform, parent) in &entrances {
        if let Some(slot) = index.slot(parent.parent()) {
            gizmos.arrow(transform.translation(), slot.position, AVAILABLE_COLOR);
        }
    }
}

/// The roles of every indexed slot, deduplicated and sorted (for the agent protocol and editor).
pub fn index_roles(index: &SmartObjectIndex) -> Vec<&str> {
    let mut set = BTreeMap::<&str, ()>::new();
    for slot in index.slots() {
        set.insert(slot.role, ());
    }
    set.into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Affine3A;
    use regia_scene::Transform;

    /// An app with `MinimalPlugins` + `SmartObjectPlugin` and one object with one slot at `pos`.
    fn app_with_slot(role: &str, pos: Vec3, enabled: bool) -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot {
                    role: role.to_string(),
                    enabled,
                    ..SmartObjectSlot::default()
                },
                GlobalTransform(Affine3A::from_translation(pos)),
                ChildOf(object),
            ))
            .id();
        (app, object, slot)
    }

    #[test]
    fn index_rebuilds_slots_with_position_role_and_facing() {
        let (mut app, object, slot) = app_with_slot("sit", Vec3::new(1.0, 2.0, 3.0), true);
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        assert_eq!(index.len(), 1);
        let info = index.slot(slot).expect("indexed");
        assert_eq!(info.role, "sit");
        assert_eq!(info.object, object);
        assert_eq!(info.position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(info.forward, Vec3::NEG_Z);
        assert!(info.available());
        assert_eq!(index_roles(index), vec!["sit"]);
    }

    #[test]
    fn nearest_available_picks_the_closest_unclaimed_slot() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let near = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new("cover"),
                GlobalTransform(Affine3A::from_translation(Vec3::new(2.0, 0.0, 0.0))),
                ChildOf(object),
            ))
            .id();
        let far = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new("cover"),
                GlobalTransform(Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0))),
                ChildOf(object),
            ))
            .id();
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        let nearest = index.nearest_available(Vec3::ZERO, "cover").expect("one");
        assert_eq!(nearest.entity, near);
        assert_eq!(index.available("cover").len(), 2);
        // Claim the near slot; the far one becomes the nearest available.
        claim_slot(app.world_mut(), near, far).expect("claims");
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        assert_eq!(
            index.nearest_available(Vec3::ZERO, "cover").expect("one").entity,
            far
        );
    }

    #[test]
    fn claim_and_release_are_checked_and_idempotent() {
        let (mut app, _object, slot) = app_with_slot("sit", Vec3::ZERO, true);
        let agent = app.world_mut().spawn_empty().id();
        let other = app.world_mut().spawn_empty().id();

        claim_slot(app.world_mut(), slot, agent).expect("first claim");
        // Idempotent for the same agent.
        claim_slot(app.world_mut(), slot, agent).expect("same agent");
        // A different agent is refused.
        assert_eq!(
            claim_slot(app.world_mut(), slot, other),
            Err(SmartObjectError::SlotClaimed(slot, agent))
        );
        // Releasing by a non-holder is refused, and the claim survives.
        assert_eq!(
            release_slot(app.world_mut(), slot, other),
            Err(SmartObjectError::NotClaimedBy(slot, agent))
        );
        release_slot(app.world_mut(), slot, agent).expect("holder releases");
        // Releasing an unclaimed slot is a no-op.
        release_slot(app.world_mut(), slot, agent).expect("no-op");
    }

    #[test]
    fn a_disabled_object_hides_its_slots() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new("sit"),
                GlobalTransform(Affine3A::from_translation(Vec3::X)),
                ChildOf(object),
            ))
            .id();
        app.update();
        assert!(
            app.world()
                .resource::<SmartObjectIndex>()
                .slot(slot)
                .expect("slot")
                .available()
        );

        app.world_mut()
            .get_mut::<SmartObject>(object)
            .expect("object")
            .enabled = false;
        app.update();
        let info = app
            .world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("slot");
        assert!(!info.enabled, "disabled object disables the slot");
        assert!(!info.available());
    }

    #[test]
    fn claim_rejects_a_disabled_slot() {
        let (mut app, _object, slot) = app_with_slot("sit", Vec3::ZERO, false);
        let agent = app.world_mut().spawn_empty().id();
        assert_eq!(
            claim_slot(app.world_mut(), slot, agent),
            Err(SmartObjectError::SlotDisabled(slot))
        );
        assert_eq!(
            claim_slot(app.world_mut(), Entity::PLACEHOLDER, agent),
            Err(SmartObjectError::SlotNotFound(Entity::PLACEHOLDER))
        );
    }

    #[test]
    fn a_slot_without_a_transform_is_not_indexed() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        // A slot with no GlobalTransform (no Transform) is not placed in the world, so the index
        // skips it rather than inventing an origin pose.
        app.world_mut().spawn(SmartObjectSlot::new("sit"));
        app.update();
        assert!(app.world().resource::<SmartObjectIndex>().is_empty());
    }

    #[test]
    fn smart_object_ancestor_walks_the_hierarchy() {
        let (mut app, object, slot) = app_with_slot("sit", Vec3::ZERO, true);
        // A middle transform node between the object and the slot still resolves to the object.
        let middle = app.world_mut().spawn(Transform::default()).id();
        app.world_mut().entity_mut(slot).insert(ChildOf(middle));
        app.world_mut().entity_mut(middle).insert(ChildOf(object));
        assert_eq!(smart_object_ancestor(app.world(), slot), Some(object));
        assert_eq!(smart_object_ancestor(app.world(), object), Some(object));
        assert_eq!(smart_object_ancestor(app.world(), middle), Some(object));
    }

    #[test]
    fn the_index_and_claims_agree_on_an_object_on_the_slot_entity() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let slot = app
            .world_mut()
            .spawn((
                SmartObject {
                    enabled: false,
                    ..SmartObject::default()
                },
                SmartObjectSlot {
                    role: "sit".to_string(),
                    enabled: true,
                    ..SmartObjectSlot::default()
                },
                GlobalTransform(Affine3A::from_translation(Vec3::ZERO)),
            ))
            .id();
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        assert!(
            index.nearest_available(Vec3::ZERO, "sit").is_none(),
            "a disabled object on the slot entity disables the slot in the index too"
        );
        let user = app.world_mut().spawn_empty().id();
        assert!(claim_slot(app.world_mut(), slot, user).is_err());
    }

    #[test]
    fn nearest_available_rejects_a_non_finite_position() {
        let (mut app, _object, _slot) = app_with_slot("sit", Vec3::X, true);
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        assert_eq!(index.nearest_available(Vec3::NAN, "sit"), None);
        assert_eq!(index.nearest_available(Vec3::splat(f32::INFINITY), "sit"), None);
    }
}
