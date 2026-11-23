// SPDX-License-Identifier: MPL-2.0
//! Formation slots (ADR-2709, `book/agents/ai-squads.md`): places around a target or the squad's leader,
//! resolved through the navigation mesh, one member to a slot.
//!
//! [`AiFormation`] goes on the squad entity. Every `interval_steps` fixed steps (and on the next step
//! after a member joins or leaves or the leader changes) the shape's candidate places are walked in a fixed order; each is projected with
//! `regia_nav::NavQuery::find_nearest`, optionally checked for a path from the anchor, and dropped when
//! it has no mesh point or lands within `min_separation` of a place already accepted. That keeps two
//! slots from collapsing onto one spot next to a wall: the shape gets more candidates (outer rings, later
//! rows) instead. With no `NavMesh` resource the places are used as they are.
//!
//! Candidates come in groups (a ring, a row), and a resolve always looks at whole groups: with two members
//! and a ring of twelve places, the two pick from all twelve instead of crowding the first two.
//!
//! **Assignment.** A member keeps its slot while the slot stays valid. The others, in join order, take the
//! nearest free slot of the innermost group with room, the earlier candidate on a tie. Each slot index
//! goes to at most one member. A member with no slot left is listed in [`AiFormation::unplaced`].
//!
//! A slotted member's own `Blackboard` gets `slot_key` (a vec3) and `<slot_key>_index` (an integer); its
//! tree walks there with whatever move leaf it uses. Both keys go away when it has no slot.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;

use crate::blackboard::{BbValue, Blackboard};
use crate::formation_asset::{AssignMember, AssignSlot, assign_slots};
use crate::squad::{AiDead, AiSquad, AiSquadClock, AiSquadMember};
use crate::tick_budget::world_pose;

/// The most candidate places looked at per resolve, and the most slots a `.formation.ron` may name.
pub const MAX_CANDIDATES: usize = 1024;

/// What a formation stands around.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum FormationAnchor {
    /// The squad's leader, who takes no slot.
    #[default]
    Leader,
    /// This entity (a target).
    Entity(Entity),
}

/// The candidate places of a formation.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default)]
pub enum FormationShape {
    /// Rings around the anchor: the first at `radius`, further ones `spacing` beyond, with `spacing`
    /// between places along each. Fixed to the world axes, so slots do not sweep as the anchor turns.
    Ring {
        /// The first ring's radius.
        radius: f32,
        /// The distance between places.
        spacing: f32,
    },
    /// Rows behind the anchor, centered on its heading.
    Line {
        /// The distance between places and between rows.
        spacing: f32,
        /// Places per row.
        row_length: u32,
    },
    /// A V opening backward from the anchor, widening by `spacing` per row.
    Wedge {
        /// The distance between rows, and the widening per row.
        spacing: f32,
    },
}

impl Default for FormationShape {
    fn default() -> Self {
        Self::Ring {
            radius: 3.0,
            spacing: 2.0,
        }
    }
}

/// One member in its slot.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default)]
pub struct FormationSlot {
    /// The candidate index: the slot's identity. Unique within the formation.
    pub index: u32,
    /// The resolved place.
    pub position: Vec3,
    /// The member standing there.
    pub member: Entity,
}

impl Default for FormationSlot {
    fn default() -> Self {
        Self {
            index: 0,
            position: Vec3::ZERO,
            member: Entity::PLACEHOLDER,
        }
    }
}

/// A squad's formation. Goes on the [`AiSquad`] entity. The last three fields are outputs. Removing it, or
/// inserting a new one over it, takes the slot keys off the members' blackboards. To rename `slot_key`,
/// insert a new value rather than writing the field.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
#[component(on_discard = formation_discarded)]
pub struct AiFormation {
    /// What the formation stands around.
    pub anchor: FormationAnchor,
    /// The candidate places.
    pub shape: FormationShape,
    /// Fixed steps between resolves (0 counts as 1).
    pub interval_steps: u32,
    /// Half-extents of the box navigation-mesh lookups search.
    pub extents: Vec3,
    /// The least distance between two slots.
    pub min_separation: f32,
    /// Also require a path on the mesh from the anchor to the slot.
    pub require_path: bool,
    /// The member blackboard key for its slot's position.
    pub slot_key: String,
    /// The members in a slot, in join order.
    pub slots: Vec<FormationSlot>,
    /// The members with no slot, in join order.
    pub unplaced: Vec<Entity>,
    /// Whether the roster or the leader changed since the last resolve.
    pub dirty: bool,
    /// Custom slot list. `None` keeps the sticky assignment of [`FormationShape`].
    ///
    /// Ignored by reflection so a scene saved before ADR-8358 still loads. Set it from code or
    /// `ai.formation_set`.
    #[reflect(ignore)]
    pub custom: Option<crate::formation_asset::FormationAsset>,
    /// Path of the `.formation.ron` last applied, when the shape came from a file.
    #[reflect(ignore)]
    pub asset_path: Option<String>,
    /// Squad-level copy of the crowd column flag. The crowd records the real flag per agent.
    #[reflect(ignore)]
    pub column: bool,
}

impl Default for AiFormation {
    fn default() -> Self {
        Self {
            anchor: FormationAnchor::Leader,
            shape: FormationShape::default(),
            interval_steps: 6,
            extents: Vec3::new(1.0, 2.0, 1.0),
            min_separation: 1.0,
            require_path: false,
            slot_key: "formation_slot".to_string(),
            slots: Vec::new(),
            unplaced: Vec::new(),
            dirty: true,
            custom: None,
            asset_path: None,
            column: false,
        }
    }
}

impl AiFormation {
    /// A formation of `shape` around `anchor`.
    pub fn new(anchor: FormationAnchor, shape: FormationShape) -> Self {
        Self {
            anchor,
            shape,
            ..Self::default()
        }
    }

    /// The blackboard key of a member's slot index.
    pub fn index_key(&self) -> String {
        format!("{}_index", self.slot_key)
    }
}

/// The candidate places as offsets in the anchor's frame (x right, z backward), in a fixed order, each
/// with its group: the ring or row it belongs to.
fn candidates(shape: FormationShape) -> impl Iterator<Item = (Vec3, u32)> {
    let positive = |v: f32| if v.is_finite() && v > 0.0 { v } else { 1.0 };
    let (mut outer, mut inner) = (0u32, 0u32);
    std::iter::from_fn(move || match shape {
        FormationShape::Ring { radius, spacing } => {
            let spacing = positive(spacing);
            let radius = radius.max(0.0);
            let ring_radius = radius + outer as f32 * spacing;
            let count = ((std::f32::consts::TAU * ring_radius / spacing).floor() as u32).max(1);
            let angle = inner as f32 * std::f32::consts::TAU / count as f32;
            // The deterministic sin/cos kernel: these offsets are written to members' blackboards
            // and projected onto the navmesh, so a platform-libm ulp would split two runs of the
            // same level (the sight cone refuses `f32::cos` for the same reason).
            let (sin, cos) = regia_core::math::det_sin_cos(angle);
            let place = (Vec3::new(cos, 0.0, sin) * ring_radius, outer);
            inner += 1;
            if inner >= count {
                inner = 0;
                outer += 1;
            }
            Some(place)
        }
        FormationShape::Line { spacing, row_length } => {
            let spacing = positive(spacing);
            let row_length = row_length.max(1);
            let x = (inner as f32 - (row_length - 1) as f32 / 2.0) * spacing;
            let place = (Vec3::new(x, 0.0, (outer + 1) as f32 * spacing), outer);
            inner += 1;
            if inner >= row_length {
                inner = 0;
                outer += 1;
            }
            Some(place)
        }
        FormationShape::Wedge { spacing } => {
            let spacing = positive(spacing);
            let row = (outer + 1) as f32 * spacing;
            let side = if inner == 0 { -1.0 } else { 1.0 };
            let place = (Vec3::new(side * row, 0.0, row), outer);
            inner += 1;
            if inner >= 2 {
                inner = 0;
                outer += 1;
            }
            Some(place)
        }
    })
}

/// Offsets of a built-in preset: the first ring, the first line row, or the first four wedge rows.
pub(crate) fn preset_offsets(shape: FormationShape) -> Vec<Vec3> {
    let mut out = Vec::new();
    for (offset, group) in candidates(shape).take(MAX_CANDIDATES) {
        let keep = match shape {
            FormationShape::Ring { .. } | FormationShape::Line { .. } => group == 0,
            FormationShape::Wedge { .. } => group < 4,
        };
        if !keep {
            break;
        }
        out.push(offset);
    }
    out
}

/// From a shape's frame to the world, around the anchor: the `Ring` keeps the world axes.
fn to_world(shape: FormationShape, anchor: Vec3, forward: Vec3, offset: Vec3) -> Vec3 {
    if matches!(shape, FormationShape::Ring { .. }) {
        return anchor + offset;
    }
    let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
    let right = Vec3::new(-flat.z, 0.0, flat.x);
    anchor + right * offset.x - flat * offset.z
}

fn clear_slot_keys(world: &mut DeferredWorld<'_>, member: Entity, slot_key: &str, index_key: &str) {
    if world
        .get::<Blackboard>(member)
        .is_some_and(|board| board.contains(slot_key) || board.contains(index_key))
        && let Some(mut board) = world.get_mut::<Blackboard>(member)
    {
        board.remove(slot_key);
        board.remove(index_key);
    }
}

fn formation_discarded(mut world: DeferredWorld<'_>, context: HookContext) {
    let Some(formation) = world.get::<AiFormation>(context.entity) else {
        return;
    };
    let (slot_key, index_key) = (formation.slot_key.clone(), formation.index_key());
    let members: Vec<Entity> = formation.slots.iter().map(|slot| slot.member).collect();
    for member in members {
        clear_slot_keys(&mut world, member, &slot_key, &index_key);
    }
}

/// The roster or the leader changed: the next squad step resolves again.
pub(crate) fn mark_dirty(world: &mut DeferredWorld<'_>, squad: Entity) {
    if let Some(mut formation) = world.get_mut::<AiFormation>(squad)
        && !formation.dirty
    {
        formation.dirty = true;
    }
}

/// A member left: its slot is free now, and the next squad step resolves again.
pub(crate) fn member_left(world: &mut DeferredWorld<'_>, squad: Entity, member: Entity) {
    let keys = world
        .get::<AiFormation>(squad)
        .map(|formation| (formation.slot_key.clone(), formation.index_key()));
    if let Some(mut formation) = world.get_mut::<AiFormation>(squad) {
        formation.slots.retain(|slot| slot.member != member);
        formation.unplaced.retain(|entity| *entity != member);
        formation.dirty = true;
    }
    if let Some((slot_key, index_key)) = keys {
        clear_slot_keys(world, member, &slot_key, &index_key);
    }
}

/// One valid place.
#[derive(Clone, Copy)]
struct Place {
    index: u32,
    position: Vec3,
    group: u32,
}

/// The valid places, in candidate order: enough for `need` members, up to the end of the group that
/// reaches it, and at least through candidate `keep_through` (the highest slot now in use).
fn resolve_places(
    world: &World,
    formation: &AiFormation,
    anchor: Vec3,
    forward: Vec3,
    need: usize,
    keep_through: u32,
) -> Vec<Place> {
    let query = world
        .get_resource::<regia_nav::NavMesh>()
        .map(|mesh| regia_nav::NavQuery {
            mesh,
            obstacles: world.get_resource::<regia_nav::NavObstacles>(),
        });
    let separation = formation.min_separation.max(1e-3);
    let mut accepted: Vec<Place> = Vec::with_capacity(need);
    let mut full_at_group = None;
    for (index, (offset, group)) in candidates(formation.shape).take(MAX_CANDIDATES).enumerate() {
        let index = index as u32;
        if accepted.len() >= need && full_at_group.is_none() {
            full_at_group = Some(group);
        }
        if full_at_group.is_some_and(|full| group > full) && index > keep_through {
            break;
        }
        let wanted = to_world(formation.shape, anchor, forward, offset);
        if !wanted.is_finite() {
            continue;
        }
        let place = match &query {
            Some(query) => {
                let Some((_, on_mesh)) = query.find_nearest(wanted, formation.extents) else {
                    continue;
                };
                if formation.require_path && query.find_path(anchor, on_mesh, formation.extents).is_none() {
                    continue;
                }
                on_mesh
            }
            None => wanted,
        };
        if accepted
            .iter()
            .any(|other| (other.position - place).length() < separation)
        {
            continue;
        }
        accepted.push(Place {
            index,
            position: place,
            group,
        });
    }
    accepted
}

/// Writes a member's slot to its blackboard, touching nothing that did not change.
fn publish(world: &mut World, member: Entity, slot_key: &str, index_key: &str, slot: Option<(u32, Vec3)>) {
    let wanted = slot.map(|(index, position)| (BbValue::Vec3(position), BbValue::Int(i64::from(index))));
    let Some(board) = world.get::<Blackboard>(member) else {
        return;
    };
    let same = match &wanted {
        Some((position, index)) => {
            board.get(slot_key) == Some(position) && board.get(index_key) == Some(index)
        }
        None => !board.contains(slot_key) && !board.contains(index_key),
    };
    if same {
        return;
    }
    if let Some(mut board) = world.get_mut::<Blackboard>(member) {
        match wanted {
            Some((position, index)) => {
                board.insert(slot_key, position);
                board.insert(index_key, index);
            }
            None => {
                board.remove(slot_key);
                board.remove(index_key);
            }
        }
    }
}

/// Resolves one formation now: the places, then who stands where (the module's assignment rule).
pub fn resolve_formation(world: &mut World, squad_entity: Entity) {
    let use_custom = world
        .get::<AiFormation>(squad_entity)
        .is_some_and(|formation| formation.custom.is_some());
    if use_custom {
        resolve_custom_formation(world, squad_entity);
        return;
    }
    let (Some(squad), Some(formation)) = (
        world.get::<AiSquad>(squad_entity),
        world.get::<AiFormation>(squad_entity),
    ) else {
        return;
    };
    let anchor_entity = match formation.anchor {
        FormationAnchor::Leader => squad.leader,
        FormationAnchor::Entity(entity) => Some(entity),
    };
    // Only this squad's own anchor: another squad's member may be the target, with a slot of its own.
    let anchor_is_member = anchor_entity.is_some_and(|anchor| squad.members.contains(&anchor));
    let members: Vec<Entity> = squad
        .members
        .iter()
        .copied()
        .filter(|member| Some(*member) != anchor_entity)
        .collect();
    let pose = anchor_entity
        .filter(|anchor| world.get::<AiDead>(*anchor).is_none())
        .and_then(|anchor| world_pose(world, anchor));
    let previous: Vec<FormationSlot> = formation.slots.clone();
    let keep_through = previous.iter().map(|slot| slot.index).max().unwrap_or(0);
    let places = match pose {
        Some((position, forward)) => {
            resolve_places(world, formation, position, forward, members.len(), keep_through)
        }
        None => Vec::new(),
    };

    // Sticky first, then in join order the nearest free place of the innermost group with room.
    let mut taken = vec![false; places.len()];
    let mut assigned: Vec<Option<usize>> = vec![None; members.len()];
    for (m, member) in members.iter().enumerate() {
        let kept = previous
            .iter()
            .find(|slot| slot.member == *member)
            .and_then(|slot| places.iter().position(|place| place.index == slot.index));
        if let Some(place) = kept
            && !taken[place]
        {
            taken[place] = true;
            assigned[m] = Some(place);
        }
    }
    for (m, member) in members.iter().enumerate() {
        if assigned[m].is_some() {
            continue;
        }
        let from = world_pose(world, *member).map_or(Vec3::ZERO, |(p, _)| p);
        let best = places
            .iter()
            .enumerate()
            .filter(|(index, _)| !taken[*index])
            .map(|(index, place)| (index, place.group, (place.position - from).length_squared()))
            .fold(None, |best: Option<(usize, u32, f32)>, row| match best {
                Some((_, group, nearest)) if (group, nearest) <= (row.1, row.2) => best,
                _ => Some(row),
            });
        if let Some((place, _, _)) = best {
            taken[place] = true;
            assigned[m] = Some(place);
        }
    }

    let mut slots = Vec::with_capacity(members.len());
    let mut unplaced = Vec::new();
    for (m, member) in members.iter().enumerate() {
        match assigned[m] {
            Some(place) => slots.push(FormationSlot {
                index: places[place].index,
                position: places[place].position,
                member: *member,
            }),
            None => unplaced.push(*member),
        }
    }
    let Some(mut formation) = world.get_mut::<AiFormation>(squad_entity) else {
        return;
    };
    formation.slots = slots;
    formation.unplaced = unplaced;
    formation.dirty = false;
    let (slot_key, index_key) = (formation.slot_key.clone(), formation.index_key());
    let slots = formation.slots.clone();
    let unplaced = formation.unplaced.clone();
    for slot in slots {
        publish(
            world,
            slot.member,
            &slot_key,
            &index_key,
            Some((slot.index, slot.position)),
        );
    }
    for member in unplaced {
        publish(world, member, &slot_key, &index_key, None);
    }
    // A leader anchor has no slot; one that just became leader may still show its old one.
    if let Some(anchor) = anchor_entity.filter(|_| anchor_is_member) {
        publish(world, anchor, &slot_key, &index_key, None);
    }
}

/// One projected custom slot.
struct CustomPlace {
    index: u32,
    fill: u32,
    position: Vec3,
    role: Option<String>,
}

/// Projects a custom asset with the anchor's facing and assigns members by minimum total distance.
fn resolve_custom_formation(world: &mut World, squad_entity: Entity) {
    let (Some(squad), Some(formation)) = (
        world.get::<AiSquad>(squad_entity),
        world.get::<AiFormation>(squad_entity),
    ) else {
        return;
    };
    let Some(asset) = formation.custom.clone() else {
        return;
    };
    let anchor_entity = match formation.anchor {
        FormationAnchor::Leader => squad.leader,
        FormationAnchor::Entity(entity) => Some(entity),
    };
    let anchor_is_member = anchor_entity.is_some_and(|anchor| squad.members.contains(&anchor));
    let members: Vec<Entity> = squad
        .members
        .iter()
        .copied()
        .filter(|member| Some(*member) != anchor_entity)
        .collect();
    let extents = formation.extents;
    let separation = formation.min_separation.max(1e-3);
    let require_path = formation.require_path;
    let pose = anchor_entity
        .filter(|anchor| world.get::<AiDead>(*anchor).is_none())
        .and_then(|anchor| world_pose(world, anchor));
    let places = match pose {
        Some((position, forward)) => project_custom_slots(
            world,
            &asset,
            position,
            forward,
            extents,
            separation,
            require_path,
        ),
        None => Vec::new(),
    };
    let assign_members: Vec<AssignMember> = members
        .iter()
        .map(|member| AssignMember {
            id: member.to_bits(),
            position: world_pose(world, *member).map_or(Vec3::ZERO, |(p, _)| p),
            role: world
                .get::<AiSquadMember>(*member)
                .and_then(|row| row.role.clone()),
        })
        .collect();
    let assign_slots_in: Vec<AssignSlot> = places
        .iter()
        .map(|place| AssignSlot {
            index: place.index,
            fill: place.fill,
            position: place.position,
            role: place.role.clone(),
        })
        .collect();
    let assigned = assign_slots(&assign_members, &assign_slots_in);
    let mut slots = Vec::with_capacity(assigned.len());
    let mut unplaced = Vec::new();
    for member in &members {
        match assigned.iter().find(|row| row.member == member.to_bits()) {
            Some(row) => {
                if let Some(place) = places.iter().find(|place| place.index == row.slot) {
                    slots.push(FormationSlot {
                        index: place.index,
                        position: place.position,
                        member: *member,
                    });
                } else {
                    unplaced.push(*member);
                }
            }
            None => unplaced.push(*member),
        }
    }
    let Some(mut formation) = world.get_mut::<AiFormation>(squad_entity) else {
        return;
    };
    formation.slots = slots;
    formation.unplaced = unplaced;
    formation.dirty = false;
    let (slot_key, index_key) = (formation.slot_key.clone(), formation.index_key());
    let slots = formation.slots.clone();
    let unplaced = formation.unplaced.clone();
    for slot in slots {
        publish(
            world,
            slot.member,
            &slot_key,
            &index_key,
            Some((slot.index, slot.position)),
        );
    }
    for member in unplaced {
        publish(world, member, &slot_key, &index_key, None);
    }
    if let Some(anchor) = anchor_entity.filter(|_| anchor_is_member) {
        publish(world, anchor, &slot_key, &index_key, None);
    }
}

fn project_custom_slots(
    world: &World,
    asset: &crate::formation_asset::FormationAsset,
    anchor: Vec3,
    forward: Vec3,
    extents: Vec3,
    separation: f32,
    require_path: bool,
) -> Vec<CustomPlace> {
    let query = world
        .get_resource::<regia_nav::NavMesh>()
        .map(|mesh| regia_nav::NavQuery {
            mesh,
            obstacles: world.get_resource::<regia_nav::NavObstacles>(),
        });
    let mut accepted = Vec::with_capacity(asset.slots.len());
    for slot in &asset.slots {
        let wanted = regia_nav::formation_offset_world(anchor, forward, slot.offset);
        if !wanted.is_finite() {
            continue;
        }
        let place = match &query {
            Some(query) => {
                let Some((_, on_mesh)) = query.find_nearest(wanted, extents) else {
                    continue;
                };
                if require_path && query.find_path(anchor, on_mesh, extents).is_none() {
                    continue;
                }
                on_mesh
            }
            None => wanted,
        };
        if accepted
            .iter()
            .any(|other: &CustomPlace| (other.position - place).length() < separation)
        {
            continue;
        }
        accepted.push(CustomPlace {
            index: slot.index,
            fill: slot.fill,
            position: place,
            role: slot.role.clone(),
        });
    }
    accepted
}

/// `FixedUpdate` (exclusive, [`crate::squad::AiSquadStep`]): advances the clock and resolves the
/// formations that are due.
pub(crate) fn step_squads(
    world: &mut World,
    query: &mut QueryState<(Entity, &'static AiFormation)>,
    mut due: Local<Vec<Entity>>,
) {
    let _span = tracing::debug_span!(target: "regia_ai", "ai_squad_step").entered();
    let step = {
        let mut clock = world.get_resource_or_insert_with(AiSquadClock::default);
        clock.step += 1;
        clock.step
    };
    due.clear();
    due.extend(query.iter(world).filter_map(|(entity, formation)| {
        let interval = u64::from(formation.interval_steps.max(1));
        (formation.dirty || step.is_multiple_of(interval)).then_some(entity)
    }));
    for &entity in due.iter() {
        resolve_formation(world, entity);
    }
}

/// `PostUpdate` overlay: a cross on each slot, a line from the member to that slot, and a line
/// from each crowd follower to its slot point (a different color while the follower is in a column).
/// [`crate::AiPlugin`] runs it only while [`regia_render::gizmos::FormationGizmos`] is enabled.
#[cfg(feature = "debug_draw")]
pub fn draw_formation_gizmos(
    formations: Query<&AiFormation>,
    transforms: Query<&regia_scene::GlobalTransform>,
    crowd: Option<Res<regia_nav::NavCrowd>>,
    mut gizmos: regia_render::gizmos::Gizmos<regia_render::gizmos::FormationGizmos>,
) {
    use regia_core::math::Color;
    const SLOT: Color = Color::rgb(0.2, 0.85, 0.35);
    const LINK: Color = Color::rgb(0.95, 0.85, 0.2);
    const COLUMN: Color = Color::rgb(0.95, 0.35, 0.15);
    let up = Vec3::Y * 0.05;
    for formation in &formations {
        for slot in &formation.slots {
            let p = slot.position + up;
            gizmos.line(
                p + Vec3::new(-0.25, 0.0, 0.0),
                p + Vec3::new(0.25, 0.0, 0.0),
                SLOT,
            );
            gizmos.line(
                p + Vec3::new(0.0, 0.0, -0.25),
                p + Vec3::new(0.0, 0.0, 0.25),
                SLOT,
            );
            if let Ok(transform) = transforms.get(slot.member) {
                gizmos.line(transform.translation() + up, p, LINK);
            }
        }
    }
    if let Some(crowd) = crowd.as_deref() {
        for agent in crowd.agents() {
            let Some(target) = agent.slot_target() else {
                continue;
            };
            let color = if agent.column() { COLUMN } else { LINK };
            gizmos.line(agent.position() + up, target + up, color);
        }
    }
}
