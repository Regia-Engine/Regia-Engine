// SPDX-License-Identifier: MIT-0
//! State-tree bots (ADR-2134): the default arena brain. Each bot runs `assets/ai/bot.statetree.ron`
//! (`regia_ai::state_tree`, spec 38) with three leaf tasks of this module:
//!
//! - [`Sense`] (ambient, every tick): writes `in_round`, `ammo`, `has_target`, `target`,
//!   `target_pos` and `target_dist` to the bot's `Blackboard`, and drops a station claim the bot no
//!   longer needs.
//! - [`Engage`] (state `Fight`): steers toward the nearest enemy and, with a clear line of sight,
//!   asks the weapon chooser what to fire and starts that ability ([`crate::loadout`]).
//! - [`Restock`] (state `Resupply`): ranks the resupply slots with the spatial query
//!   `assets/ai/resupply.spatialquery.ron` ([`STATION_QUERY`], `regia_ai::spatial_query`), claims
//!   the first free one (a smart object, `regia_ai::smart_object_v2`), walks to it, uses it for [`RESUPPLY_SECS`] and applies the refill
//!   effect, then succeeds (once the new `ammo` reads back) so the tree goes back to `Fight`.
//!
//! Everything runs in the fixed step (`AiStep`), draws randomness only from [`MatchRng`] and visits
//! pawns and slots in entity order, so a seed replays to the same match (`tests/repro_check.rs`).

use regia_ai::spatial_query::{Pick, QueryStatus, SpatialQueryManager, SpatialQueryRequest};
use regia_ai::{
    BtContext, BtStatus, BtTask, ClaimHandle, ClaimPriority, SmartObject, SmartObjectSlot, StateTree,
    StateTreeDef, StateTreeInbox,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::{GlobalTransform, Transform};

use crate::config::MatchRng;
use crate::loadout::{self, WeaponPick};
use crate::pawn::{BotBrain, PAWN_EYE_HEIGHT, Pawn};
use crate::weapons::{FireRequest, Weapon};

/// The bot tree's asset path (what `statetree.compile` / `statetree.reload` take).
pub const BOT_TREE: &str = "ai/bot.statetree.ron";
/// The bot tree's text.
pub const BOT_TREE_TEXT: &str = include_str!("../assets/ai/bot.statetree.ron");
/// The smart-object slot role of a resupply station.
pub const RESUPPLY_ROLE: &str = "resupply";
/// The scene tag (`regia_scene::components::Tags`) on every resupply slot; the station query's
/// generator matches it.
pub const SLOT_TAG: &str = "resupply_slot";
/// The station query's template name (what `ai.query_run` takes).
pub const STATION_QUERY: &str = "resupply_stations";
/// The station query's text (`assets/ai/resupply.spatialquery.ron`).
pub const STATION_QUERY_TEXT: &str = include_str!("../assets/ai/resupply.spatialquery.ron");
/// Seconds a bot stands at a station before the refill applies.
pub const RESUPPLY_SECS: f32 = 0.5;
/// Distance at which a bot counts as standing on a slot (m).
pub const SLOT_REACH: f32 = 0.6;
/// Bot steering speed (m/s), as the steering bots.
const BOT_SPEED: f32 = 3.5;
/// Maximum distance a bot will engage an enemy (m).
const ENGAGE_RANGE: f32 = 30.0;
/// A bot stops closing in at this distance (m) and holds its ground.
const STANDOFF: f32 = 6.0;
/// The station positions on the floor (two, on opposite sides of the centre).
pub const STATION_SPOTS: [Vec3; 2] = [Vec3::new(-6.0, 0.0, 3.0), Vec3::new(6.0, 0.0, -3.0)];

/// Registers the three leaf tasks into the `BtTaskRegistry`.
pub fn register_tasks(world: &mut World) {
    let mut registry = world.get_resource_or_insert_with::<regia_ai::BtTaskRegistry>(Default::default);
    registry.register::<Sense>();
    registry.register::<Engage>();
    registry.register::<Restock>();
}

/// Attaches a compiled bot tree, a blackboard and an inbox to every bot pawn.
///
/// # Errors
///
/// The parse or compile error of the embedded tree (no bot gets a tree then).
pub fn setup_state_tree_bots(world: &mut World) -> Result<usize, String> {
    register_tasks(world);
    let def = StateTreeDef::from_ron(BOT_TREE_TEXT).map_err(|e| e.to_string())?;
    let mut bots: Vec<Entity> = {
        let mut query = world.query::<(Entity, &Pawn)>();
        query
            .iter(world)
            .filter(|(_, pawn)| pawn.is_bot)
            .map(|(entity, _)| entity)
            .collect()
    };
    bots.sort_by_key(|e| e.index());
    let mut trees = Vec::with_capacity(bots.len());
    {
        let registry = world.resource::<regia_ai::BtTaskRegistry>();
        for _ in &bots {
            trees.push(StateTree::compile(&def, registry).map_err(|e| e.to_string())?);
        }
    }
    let count = bots.len();
    for (bot, tree) in bots.into_iter().zip(trees) {
        world
            .entity_mut(bot)
            .insert((regia_ai::Blackboard::default(), StateTreeInbox::default(), tree));
    }
    Ok(count)
}

/// Registers the station query on the world's `SpatialQueryManager` (installed by
/// `SpatialQueryPlugin`).
///
/// # Errors
///
/// No manager in the world, or the embedded query does not load.
pub fn register_station_query(world: &mut World) -> Result<(), String> {
    let Some(mut manager) = world.get_resource_mut::<SpatialQueryManager>() else {
        return Err("no SpatialQueryManager (SpatialQueryPlugin is not installed)".into());
    };
    manager
        .register_template_text(STATION_QUERY_TEXT)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Spawns the resupply stations: a short static block with a [`SmartObject`] and one child
/// [`SmartObjectSlot`] (role [`RESUPPLY_ROLE`]) on the side facing the arena centre.
pub fn spawn_stations(world: &mut World) -> Vec<Entity> {
    let mut slots = Vec::new();
    for (i, spot) in STATION_SPOTS.iter().enumerate() {
        let block = crate::meshes::spawn_static_visible(
            world,
            &format!("resupply_{i}"),
            Vec3::new(spot.x, 0.4, spot.z),
            Vec3::new(0.5, 0.4, 0.5),
            [0.95, 0.75, 0.15, 1.0],
        );
        world.entity_mut(block).insert(SmartObject {
            tags: vec![RESUPPLY_ROLE.to_string()],
            ..SmartObject::default()
        });
        // The slot stands 1 m from the block toward the centre, facing the block.
        let toward_centre = (-*spot).normalize_or_zero();
        let local = Vec3::new(toward_centre.x, -0.4, toward_centre.z);
        let world_pos = Vec3::new(spot.x, 0.0, spot.z) + toward_centre;
        let slot_transform = Transform::from_translation(world_pos);
        let slot = world
            .spawn((
                regia_scene::components::Name::new(format!("resupply_{i}_slot")),
                regia_scene::components::Tags::from_iter_str([SLOT_TAG]),
                Transform::from_translation(local),
                GlobalTransform(slot_transform.compute_affine()),
                SmartObjectSlot::new(RESUPPLY_ROLE),
                ChildOf(block),
            ))
            .id();
        slots.push(slot);
    }
    slots
}

/// The nearest alive enemy of `me` within `ENGAGE_RANGE`: `(entity, position, distance)`.
/// Pawns are visited in entity order and ties keep the earlier pawn.
pub fn nearest_enemy(world: &mut World, me: Entity) -> Option<(Entity, Vec3, f32)> {
    let own_slot = world.get::<Pawn>(me).map_or(u8::MAX, |p| p.slot);
    let own_pos = world.get::<Transform>(me).map(|t| t.translation)?;
    let mut candidates: Vec<(Entity, Vec3)> = {
        let mut query = world.query::<(Entity, &Pawn, &Transform)>();
        query
            .iter(world)
            .filter(|(entity, pawn, _)| *entity != me && pawn.slot != own_slot)
            .map(|(entity, _, transform)| (entity, transform.translation))
            .collect()
    };
    candidates.sort_by_key(|(entity, _)| entity.index());
    let mut best: Option<(Entity, Vec3, f32)> = None;
    for (entity, pos) in candidates {
        if !crate::pawn::pawn_is_alive(world, entity) {
            continue;
        }
        let dist = (pos - own_pos).length();
        if dist <= ENGAGE_RANGE && best.is_none_or(|(_, _, d)| dist < d) {
            best = Some((entity, pos, dist));
        }
    }
    best
}

/// Moves the agent toward `destination` at bot speed (clamped to the arena) and returns the
/// remaining distance.
fn steer_toward(ctx: &mut BtContext<'_>, destination: Vec3) -> f32 {
    let Some(pos) = ctx.position() else {
        return f32::INFINITY;
    };
    let flat = Vec3::new(destination.x, pos.y, destination.z);
    let delta = flat - pos;
    let dist = delta.length();
    if dist > 1.0e-4 {
        let step = (BOT_SPEED * ctx.dt).min(dist);
        let next = crate::bots::clamp_to_arena(pos + delta / dist * step);
        ctx.set_position(next);
    }
    (flat - ctx.position().unwrap_or(pos)).length()
}

/// The slots `me` holds a claim on, in entity order.
fn held_slots(world: &mut World, me: Entity) -> Vec<Entity> {
    let mut query = world.query::<(Entity, &SmartObjectSlot)>();
    let mut held: Vec<Entity> = query
        .iter(world)
        .filter(|(_, slot)| slot.claimed_by == Some(me))
        .map(|(entity, _)| entity)
        .collect();
    held.sort_by_key(|e| e.index());
    held
}

/// Ambient sensing task (see the module docs).
#[derive(Default, regia_ai::BtTask)]
pub struct Sense;

impl BtTask for Sense {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let in_round = crate::match_flow::arena_state(ctx.world)
            .is_some_and(|s| s.phase == crate::match_flow::Phase::Round);
        let ammo = loadout::ammo(ctx.world, ctx.entity);
        let target = nearest_enemy(ctx.world, ctx.entity);
        let resupplying = ctx
            .blackboard()
            .is_some_and(|bb| bb.get_string("state") == "Resupply");
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("in_round", in_round);
            bb.insert("ammo", ammo);
            bb.insert("has_target", target.is_some());
            if let Some((entity, pos, dist)) = target {
                bb.insert("target", entity);
                bb.insert("target_pos", pos);
                bb.insert("target_dist", dist);
            }
        }
        // A claim outlives the `Resupply` state when the round ends mid-walk: hand it back.
        if !resupplying {
            for slot in held_slots(ctx.world, ctx.entity) {
                let _ = regia_ai::release_slot(ctx.world, slot, ctx.entity);
            }
        }
        BtStatus::Running
    }
}

/// The `Fight` task: close in, pick a weapon with the chooser, fire through the abilities.
#[derive(Default, regia_ai::BtTask)]
pub struct Engage;

impl BtTask for Engage {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let (has_target, target, target_pos) = match ctx.blackboard() {
            Some(bb) => (
                bb.get_bool("has_target"),
                bb.get_entity("target"),
                bb.get_vec3("target_pos"),
            ),
            None => (false, Entity::PLACEHOLDER, Vec3::ZERO),
        };
        if !has_target {
            // Nobody in range: drift toward a seeded random point near the centre.
            let jitter = {
                let mut rng = ctx.world.resource_mut::<MatchRng>();
                Vec3::new(rng.rng.f32() - 0.5, 0.0, rng.rng.f32() - 0.5) * 4.0
            };
            steer_toward(ctx, jitter);
            if let Some(mut brain) = ctx.world.get_mut::<BotBrain>(ctx.entity) {
                brain.target = None;
            }
            return BtStatus::Running;
        }
        let pos = ctx.position().unwrap_or(Vec3::ZERO);
        if (target_pos - pos).length() > STANDOFF {
            steer_toward(ctx, target_pos);
        }
        let eye = ctx.position().unwrap_or(pos);
        let dir = (target_pos - eye).normalize_or_zero();
        let distance = (target_pos - eye).length();
        let clear = crate::bot_bt::line_of_sight_clear(ctx.world, eye, dir, ENGAGE_RANGE, ctx.entity)
            .is_some_and(|hit| hit.entity == target);
        if let Some(mut brain) = ctx.world.get_mut::<BotBrain>(ctx.entity) {
            brain.target = Some(target);
        }
        if !clear {
            return BtStatus::Running;
        }
        let pick = loadout::pick_weapon(ctx.world, ctx.entity, distance);
        if pick == WeaponPick::Hold {
            return BtStatus::Running;
        }
        let Some(weapon) = loadout::trigger(ctx.world, ctx.entity, pick) else {
            return BtStatus::Running;
        };
        // Seeded spread on rifles; the draw happens for both so the stream does not depend on the
        // weapon (as the steering bots).
        let jitter = {
            let mut rng = ctx.world.resource_mut::<MatchRng>();
            Vec3::new(rng.rng.f32() - 0.5, rng.rng.f32() - 0.5, 0.0) * 0.02
        };
        let aim = if weapon == Weapon::Rifle {
            (dir + jitter).normalize_or_zero()
        } else {
            dir
        };
        ctx.world.write_message(FireRequest {
            shooter: ctx.entity,
            position: eye,
            direction: aim,
            weapon,
        });
        if let Some(mut brain) = ctx.world.get_mut::<BotBrain>(ctx.entity) {
            brain.shots = brain.shots.saturating_add(1);
        }
        BtStatus::Running
    }
}

/// The `Resupply` task: claim, walk, use, refill, release.
#[derive(Default, regia_ai::BtTask)]
pub struct Restock {
    /// The held claim.
    handle: Option<ClaimHandle>,
    /// Seconds spent using the slot.
    used: f32,
    /// The refill has applied; the task succeeds once `ammo` reads it (the attribute's `current`
    /// is recomputed in the next fixed step, and `Fight` checks it on entry).
    refilled: bool,
}

impl Restock {
    /// The first free resupply slot in the station query's ranking for `me` (nearest first, ties:
    /// lower entity id); `None` when every slot is busy or the query cannot run.
    fn find_slot(world: &mut World, me: Entity) -> Option<Entity> {
        let ranked = world.try_resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
            manager.run_instant(
                world,
                SpatialQueryRequest::new(STATION_QUERY, me).with_pick(Pick::All),
            )
        })?;
        if ranked.status != QueryStatus::Success {
            return None;
        }
        ranked.items.iter().filter_map(|item| item.entity).find(|slot| {
            world
                .get::<SmartObjectSlot>(*slot)
                .is_some_and(|s| s.role == RESUPPLY_ROLE)
                && regia_ai::smart_object_v2::slot_state(world, *slot) == Some(regia_ai::SlotState::Free)
        })
    }

    fn drop_claim(&mut self, world: &mut World) {
        if let Some(handle) = self.handle.take() {
            let _ = regia_ai::release_handle(world, handle);
        }
        self.used = 0.0;
    }
}

impl BtTask for Restock {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if self.refilled {
            if loadout::ammo(ctx.world, ctx.entity) >= 1.0 {
                self.refilled = false;
                return BtStatus::Success;
            }
            return BtStatus::Running;
        }
        // A claim can be lost (preempted, the station disabled): look again.
        if let Some(handle) = self.handle
            && ctx
                .world
                .get::<SmartObjectSlot>(handle.slot)
                .is_none_or(|slot| slot.claimed_by != Some(ctx.entity))
        {
            self.handle = None;
            self.used = 0.0;
        }
        if self.handle.is_none() {
            let Some(slot) = Self::find_slot(ctx.world, ctx.entity) else {
                return BtStatus::Running; // every station busy: wait
            };
            match regia_ai::claim_with(ctx.world, slot, ctx.entity, ClaimPriority::Normal) {
                Ok(handle) => self.handle = Some(handle),
                Err(_) => return BtStatus::Running,
            }
        }
        let Some(handle) = self.handle else {
            return BtStatus::Running;
        };
        let slot_pos = ctx
            .world
            .get::<GlobalTransform>(handle.slot)
            .map(GlobalTransform::translation)
            .unwrap_or(Vec3::ZERO);
        let remaining = steer_toward(ctx, Vec3::new(slot_pos.x, PAWN_EYE_HEIGHT, slot_pos.z));
        if remaining > SLOT_REACH {
            return BtStatus::Running;
        }
        if self.used == 0.0 && regia_ai::begin_use(ctx.world, handle).is_err() {
            self.drop_claim(ctx.world);
            return BtStatus::Running;
        }
        self.used += ctx.dt;
        if self.used + 1.0e-6 < RESUPPLY_SECS {
            return BtStatus::Running;
        }
        let station = ctx.world.get::<ChildOf>(handle.slot).map(ChildOf::parent);
        self.refilled = loadout::refill(ctx.world, ctx.entity, station);
        self.drop_claim(ctx.world);
        BtStatus::Running
    }

    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        self.drop_claim(ctx.world);
        self.refilled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_bot_tree_compiles_against_the_registered_tasks() {
        let mut world = World::new();
        register_tasks(&mut world);
        let def = StateTreeDef::from_ron(BOT_TREE_TEXT).expect("parses");
        let tree = StateTree::compile(&def, world.resource::<regia_ai::BtTaskRegistry>()).expect("compiles");
        for state in ["Bot", "Idle", "Fight", "Resupply"] {
            assert!(tree.has_state(state), "{state}");
        }
    }

    #[test]
    fn the_station_query_ranks_the_nearer_slot_first_and_restock_skips_a_busy_one() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_ai::SmartObjectPlugin,
            regia_ai::SpatialQueryPlugin,
        ));
        assert!(
            register_station_query(app.world_mut()).is_ok(),
            "the embedded query loads"
        );
        let slots = spawn_stations(app.world_mut());
        // The query reads `GlobalTransform` (what the scene propagates each frame), so the test
        // askers carry one that matches their `Transform`.
        let posed = |at: Vec3| {
            let transform = Transform::from_translation(at);
            (transform, GlobalTransform(transform.compute_affine()))
        };
        // A bot beside the second station.
        let near_second = app.world_mut().spawn(posed(Vec3::new(5.0, 1.0, -2.0))).id();
        let ranked = app
            .world_mut()
            .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
                manager.run_instant(world, SpatialQueryRequest::new(STATION_QUERY, near_second))
            });
        assert_eq!(ranked.status, QueryStatus::Success);
        let order: Vec<Entity> = ranked.items.iter().filter_map(|i| i.entity).collect();
        assert_eq!(order, vec![slots[1], slots[0]], "nearest first");
        assert_eq!(Restock::find_slot(app.world_mut(), near_second), Some(slots[1]));
        // The nearer slot taken: the far one is the pick; both taken: none.
        let other = app.world_mut().spawn_empty().id();
        let held = regia_ai::claim_with(app.world_mut(), slots[1], other, ClaimPriority::Normal);
        assert!(held.is_ok());
        assert_eq!(Restock::find_slot(app.world_mut(), near_second), Some(slots[0]));
        let held = regia_ai::claim_with(app.world_mut(), slots[0], other, ClaimPriority::Normal);
        assert!(held.is_ok());
        assert_eq!(Restock::find_slot(app.world_mut(), near_second), None);
        // Equal distance: the lower entity id wins (generation order).
        let middle = app.world_mut().spawn(posed(Vec3::ZERO)).id();
        let tie = app
            .world_mut()
            .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
                manager.run_instant(world, SpatialQueryRequest::new(STATION_QUERY, middle))
            });
        let first = tie.items.first().and_then(|i| i.entity);
        let lower = slots.iter().copied().min_by_key(|e| e.index_u32());
        assert_eq!(first, lower, "a tie goes to the lower entity index");
    }

    #[test]
    fn without_the_query_plugin_registration_is_an_error_not_a_panic() {
        let mut world = World::new();
        assert!(register_station_query(&mut world).is_err());
        assert_eq!(Restock::find_slot(&mut world, Entity::PLACEHOLDER), None);
    }

    #[test]
    fn stations_offer_one_resupply_slot_each_facing_the_centre() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_ai::SmartObjectPlugin));
        let slots = spawn_stations(app.world_mut());
        assert_eq!(slots.len(), STATION_SPOTS.len());
        for (slot, spot) in slots.iter().zip(STATION_SPOTS) {
            let pos = app
                .world()
                .get::<GlobalTransform>(*slot)
                .expect("slot pose")
                .translation();
            assert!(
                pos.length() < spot.length(),
                "the slot {pos} is nearer the centre than its block {spot}"
            );
            assert_eq!(
                app.world().get::<SmartObjectSlot>(*slot).map(|s| s.role.as_str()),
                Some(RESUPPLY_ROLE)
            );
        }
    }
}
