// SPDX-License-Identifier: MPL-2.0
//! Spawn-point selection over the ECS world (ADR-8647).
//!
//! [`choose_spawn`] gathers every [`PlayerStart`] (with its optional [`SpawnPoint`]) in spawn order,
//! every contestant (an entity with [`TeamId`] and a [`Transform`]; alive unless it carries
//! [`Died`]), and the asking player's team, then runs the plain-data picker
//! ([`crate::rules::spawn::pick_spawn`]) with the hosted session's policy and state when a
//! [`MatchRules`] resource exists, else [`SpawnPolicy::First`]. Sight lines use a physics raycast
//! when the world has a [`regia_physics::PhysicsWorld`]; the blocked-point overlap test runs only
//! when a [`SpawnShape`] resource turns it on. A start carrying [`PlayerStartDisabled`] counts as a
//! disabled point (refused `disabled`, still listed by the preview). With no [`SpawnPoint`]s, no [`SpawnShape`] and no
//! session policy this is S-14's "first start in spawn order".
//!
//! Spawn order is the arrival order of `PlayerStart` when the world tracks it
//! (`regia_ecs::spawn_order::SpawnOrder<PlayerStart>`, installed by
//! [`crate::player::PlayerPlugin`]), else position order (x, then y, then z). Neither depends on
//! entity indices, which Bevy recycles ([`StartKey`], ADR-8704).
//!
//! [`spawn_preview`] returns every point's eligibility and score terms (the agent protocol's
//! `rules.spawn_preview`, `book/agents/runtime-gameplay.md`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::queries::{QueryFilter, WorldQueries};
use regia_physics::{ColliderShape, ShapeAxis};
use regia_scene::Transform;

use crate::damage::Died;
use crate::player::{PlayerController, PlayerStart, PlayerStartDisabled, SpawnPoint};
use crate::rules::objective::Contestant;
use crate::rules::spawn::{
    SpawnAsk, SpawnChoice, SpawnPointData, SpawnPolicy, SpawnState, SpawnTerms, SpawnWeights, pick_spawn,
    preview_spawn,
};
use crate::rules::{MatchRules, TeamId};

/// The pawn shape and eye height the ECS spawn queries use (ADR-8647).
///
/// Absent, the defaults apply and no overlap test runs (S-14 picks blocked starts too).
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Resource, Default)]
pub struct SpawnShape {
    /// Capsule radius of the pawn, meters.
    pub radius: f32,
    /// Half height of the capsule's cylinder, meters.
    pub half_height: f32,
    /// Height above a point and above an enemy's position that sight rays run at, meters.
    pub eye_height: f32,
    /// Skip a point where the pawn's capsule overlaps a collider (or, with no physics world, a
    /// living contestant within two radii).
    pub block_test: bool,
}

impl Default for SpawnShape {
    fn default() -> Self {
        Self {
            radius: 0.4,
            half_height: 0.5,
            eye_height: 1.6,
            block_test: false,
        }
    }
}

/// The start [`choose_spawn`] picked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpawnPick {
    /// The [`PlayerStart`] entity.
    pub entity: Entity,
    /// Its translation.
    pub position: Vec3,
    /// The picker's answer.
    pub choice: SpawnChoice,
}

/// One row of [`spawn_preview`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPreviewRow {
    /// The [`PlayerStart`] entity.
    pub entity: Entity,
    /// Its translation.
    pub position: Vec3,
    /// Its selection data (the default when it has no [`SpawnPoint`]).
    pub point: SpawnPoint,
    /// Eligibility and score terms.
    pub terms: SpawnTerms,
}

/// What [`spawn_preview`] saw.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPreview {
    /// The ask (team, side, tick, clearance).
    pub ask: SpawnAsk,
    /// The policy a pick would use.
    pub policy: SpawnPolicy,
    /// Every start, in walk order.
    pub rows: Vec<SpawnPreviewRow>,
    /// The start a pick would choose now, without consuming the session's state.
    pub pick: Option<Entity>,
    /// True when the walk follows `PlayerStart` arrival order (the world tracks it); false when it
    /// follows position order ([`StartKey`], ADR-8704).
    pub arrival_order: bool,
}

struct Gathered {
    starts: Vec<(Entity, Vec3, SpawnPoint)>,
    points: Vec<SpawnPointData>,
    units: Vec<Contestant>,
    unit_entities: Vec<Entity>,
    ask: SpawnAsk,
    shape: SpawnShape,
}

fn contestant_id(entity: Entity) -> u32 {
    entity.index().index().saturating_add(1)
}

/// Sort key of one [`PlayerStart`] in spawn order (ADR-8704).
///
/// With `SpawnOrder<PlayerStart>` tracking (what [`crate::player::PlayerPlugin`] installs) the
/// arrival number comes first. Without it (a bare `World`, or a game that took the component's
/// hooks) every start has the same `u64::MAX` and the order is the translation: x, then y, then z,
/// in IEEE total order. Map data puts the same positions on every peer, and a recycled entity index
/// does not move a start. The entity bits only break an exact tie of two untracked starts at one
/// position.
pub type StartKey = (u64, u32, u32, u32, u64);

/// IEEE total order of `v` as an unsigned key (the order `f32::total_cmp` uses).
fn total_order_bits(v: f32) -> u32 {
    let bits = v.to_bits();
    if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    }
}

/// The [`StartKey`] of `entity` at `translation` under `order` (the world's
/// `SpawnOrder<PlayerStart>`, if any). Only a tracking order's arrival number is used.
pub fn start_key(
    order: Option<&regia_ecs::spawn_order::SpawnOrder<PlayerStart>>,
    entity: Entity,
    translation: Vec3,
) -> StartKey {
    let arrival = order
        .filter(|o| o.is_tracking())
        .and_then(|o| o.get(entity))
        .unwrap_or(u64::MAX);
    (
        arrival,
        total_order_bits(translation.x),
        total_order_bits(translation.y),
        total_order_bits(translation.z),
        entity.to_bits(),
    )
}

/// True when the world numbers [`PlayerStart`] arrivals, so spawn order is arrival order; false
/// means the position order of [`StartKey`].
pub fn starts_in_arrival_order(world: &World) -> bool {
    world
        .get_resource::<regia_ecs::spawn_order::SpawnOrder<PlayerStart>>()
        .is_some_and(|o| o.is_tracking())
}

/// The asking player's pawn (its possessed entity) and team: [`TeamId`] on the player, else on the
/// pawn, else 0.
fn player_team(world: &World, player: Entity) -> (Option<Entity>, u8) {
    let pawn = world
        .get::<PlayerController>(player)
        .map(|c| c.possessed)
        .filter(|p| *p != Entity::PLACEHOLDER);
    let team = world
        .get::<TeamId>(player)
        .or_else(|| pawn.and_then(|p| world.get::<TeamId>(p)))
        .map_or(0, |t| t.0);
    (pawn, team)
}

fn gather(world: &mut World, player: Entity) -> Gathered {
    let mut starts_q = world
        .query_filtered::<(Entity, &Transform, Option<&SpawnPoint>, Has<PlayerStartDisabled>), With<PlayerStart>>();
    let mut units_q = world.query::<(Entity, &Transform, &TeamId, Has<Died>)>();
    let world: &World = world;
    let order = world.get_resource::<regia_ecs::spawn_order::SpawnOrder<PlayerStart>>();
    let mut starts: Vec<(Entity, Vec3, SpawnPoint)> = starts_q
        .iter(world)
        .map(|(e, t, p, disabled)| {
            let mut point = p.cloned().unwrap_or_default();
            // A logic graph's `rules::SetSpawnEnabled` (ADR-8682) takes a start out with
            // `PlayerStartDisabled`: the picker sees it as a disabled point (ADR-8731).
            point.enabled &= !disabled;
            (e, t.translation, point)
        })
        .collect();
    starts.sort_by_key(|(entity, position, _)| start_key(order, *entity, *position));
    let mut units: Vec<(Entity, Contestant)> = units_q
        .iter(world)
        .map(|(e, t, team, died)| {
            (
                e,
                Contestant {
                    id: contestant_id(e),
                    team: team.0,
                    position: t.translation.to_array(),
                    alive: !died,
                },
            )
        })
        .collect();
    units.sort_by_key(|(e, _)| e.to_bits());
    let (pawn, team) = player_team(world, player);
    let shape = world.get_resource::<SpawnShape>().copied().unwrap_or_default();
    let has_physics = world.get_resource::<regia_physics::PhysicsWorld>().is_some();
    let session = world.get_resource::<MatchRules>().map(|r| &r.session);
    let mut ask = match session {
        Some(session) => session.spawn_ask(team),
        None => SpawnAsk {
            team,
            side: team,
            ..SpawnAsk::default()
        },
    };
    ask.id = contestant_id(pawn.unwrap_or(player));
    if shape.block_test && !has_physics {
        ask.clearance = shape.radius * 2.0;
    }
    let queries = world
        .get_resource::<regia_physics::PhysicsWorld>()
        .map(|w| WorldQueries {
            world: w,
            layers: world.get_resource::<regia_physics::PhysicsLayers>(),
        });
    let exclude: Vec<Entity> = pawn.into_iter().chain(Some(player)).collect();
    let points = starts
        .iter()
        .enumerate()
        .map(|(rank, (entity, position, point))| {
            let blocked = shape.block_test
                && queries.as_ref().is_some_and(|q| {
                    let capsule = ColliderShape::Capsule {
                        half_height: shape.half_height,
                        radius: shape.radius,
                        axis: ShapeAxis::Y,
                    };
                    let center = *position + Vec3::Y * (shape.half_height + shape.radius);
                    let filter = QueryFilter::new().exclude_sensors().exclude_entities(&exclude);
                    !q.overlap(&capsule, Transform::from_translation(center), filter)
                        .is_empty()
                });
            SpawnPointData {
                id: contestant_id(*entity),
                order: rank as u64,
                position: position.to_array(),
                team: point.team,
                tags: point.tags.clone(),
                priority: point.priority,
                enabled: point.enabled,
                blocked,
            }
        })
        .collect();
    let (unit_entities, units) = units.into_iter().unzip();
    Gathered {
        starts,
        points,
        units,
        unit_entities,
        ask,
        shape,
    }
}

/// The sight test: a ray between the two eye points that hits anything but a contestant is blocked.
fn occluded_by_physics<'a>(
    world: &'a World,
    exclude: &'a [Entity],
    eye: f32,
) -> impl FnMut([f32; 3], [f32; 3]) -> bool + 'a {
    let queries = world
        .get_resource::<regia_physics::PhysicsWorld>()
        .map(|w| WorldQueries {
            world: w,
            layers: world.get_resource::<regia_physics::PhysicsLayers>(),
        });
    move |a: [f32; 3], b: [f32; 3]| {
        let Some(q) = queries.as_ref() else {
            return false;
        };
        let from = Vec3::from_array(a) + Vec3::Y * eye;
        let to = Vec3::from_array(b) + Vec3::Y * eye;
        let span = to - from;
        let length = span.length();
        if length <= 1e-4 {
            return false;
        }
        let filter = QueryFilter::new().exclude_sensors().exclude_entities(exclude);
        q.raycast(from, span / length, length, filter)
            .is_some_and(|hit| hit.distance < length - 1e-3)
    }
}

fn policy_and_state(world: &World) -> (SpawnPolicy, SpawnState) {
    match world.get_resource::<MatchRules>() {
        Some(rules) => (rules.session.respawn.spawn, rules.session.spawn.clone()),
        None => (SpawnPolicy::First, SpawnState::default()),
    }
}

/// Choose a [`PlayerStart`] for `player` (see the module docs). The session's spawn state (round-robin
/// cursor, recent uses, generator) advances when a [`MatchRules`] resource exists. `None` when no
/// start is eligible.
pub fn choose_spawn(world: &mut World, player: Entity) -> Option<SpawnPick> {
    let _span = tracing::debug_span!("spawn_choose").entered();
    let g = gather(world, player);
    let (policy, mut state) = policy_and_state(world);
    let mut exclude = g.unit_entities.clone();
    exclude.push(player);
    let choice = {
        let occluded = occluded_by_physics(world, &exclude, g.shape.eye_height);
        pick_spawn(&g.points, &g.ask, &g.units, &policy, &mut state, occluded)?
    };
    if let Some(mut rules) = world.get_resource_mut::<MatchRules>() {
        rules.session.spawn = state;
    }
    let (entity, position, _) = g.starts.get(choice.index)?;
    Some(SpawnPick {
        entity: *entity,
        position: *position,
        choice,
    })
}

/// Every start's eligibility and score terms for `player`, plus the start a pick would choose now.
/// Reads only: the session's spawn state does not move.
pub fn spawn_preview(world: &mut World, player: Entity) -> SpawnPreview {
    let g = gather(world, player);
    let (policy, state) = policy_and_state(world);
    let weights = match policy {
        SpawnPolicy::Scored(w) => w,
        _ => SpawnWeights::default(),
    };
    let mut exclude = g.unit_entities.clone();
    exclude.push(player);
    let terms = preview_spawn(
        &g.points,
        &g.ask,
        &g.units,
        &weights,
        &state,
        occluded_by_physics(world, &exclude, g.shape.eye_height),
    );
    let mut trial = state;
    let pick = pick_spawn(
        &g.points,
        &g.ask,
        &g.units,
        &policy,
        &mut trial,
        occluded_by_physics(world, &exclude, g.shape.eye_height),
    )
    .and_then(|c| g.starts.get(c.index).map(|s| s.0));
    let rows = terms
        .into_iter()
        .filter_map(|t| {
            let at = g.points.iter().position(|p| p.id == t.id)?;
            let (entity, position, point) = g.starts.get(at)?.clone();
            Some(SpawnPreviewRow {
                entity,
                position,
                point,
                terms: t,
            })
        })
        .collect();
    SpawnPreview {
        ask: g.ask,
        policy,
        rows,
        pick,
        arrival_order: starts_in_arrival_order(world),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::session::{MatchSession, SessionSettings};
    use crate::rules::spawn::RespawnRules;

    fn start(world: &mut World, at: Vec3, point: Option<SpawnPoint>) -> Entity {
        let mut e = world.spawn((Transform::from_translation(at), PlayerStart));
        if let Some(p) = point {
            e.insert(p);
        }
        e.id()
    }

    fn session_with(policy: SpawnPolicy) -> MatchRules {
        MatchRules {
            session: MatchSession::new(SessionSettings {
                respawn: RespawnRules {
                    spawn: policy,
                    ..RespawnRules::default()
                },
                ..SessionSettings::default()
            }),
        }
    }

    #[test]
    fn no_spawn_data_keeps_the_first_start_in_spawn_order() {
        let mut world = World::new();
        let a = start(&mut world, Vec3::new(1.0, 0.0, 0.0), None);
        start(&mut world, Vec3::new(2.0, 0.0, 0.0), None);
        let player = world.spawn_empty().id();
        let pick = choose_spawn(&mut world, player).expect("a start");
        assert_eq!(pick.entity, a);
        assert_eq!(
            pick.position,
            crate::rules::default_spawn_point(&mut world, player)
        );
    }

    #[test]
    fn ecs_team_spawn_uses_the_players_side_and_skips_enemy_points() {
        let mut world = World::new();
        start(
            &mut world,
            Vec3::new(0.0, 0.0, 0.0),
            Some(SpawnPoint::for_team(1)),
        );
        let blue = start(
            &mut world,
            Vec3::new(9.0, 0.0, 0.0),
            Some(SpawnPoint::for_team(2)),
        );
        let player = world.spawn(TeamId(2)).id();
        world.insert_resource(session_with(SpawnPolicy::RoundRobin));
        for _ in 0..4 {
            assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(blue));
        }
    }

    #[test]
    fn ecs_scored_spawn_avoids_the_enemy_and_preview_lists_every_point() {
        let mut world = World::new();
        let near = start(&mut world, Vec3::new(1.0, 0.0, 0.0), None);
        let far = start(&mut world, Vec3::new(30.0, 0.0, 0.0), None);
        let off = SpawnPoint {
            enabled: false,
            ..SpawnPoint::default()
        };
        let disabled = start(&mut world, Vec3::new(60.0, 0.0, 0.0), Some(off));
        world.spawn((Transform::default(), TeamId(2)));
        let player = world.spawn(TeamId(1)).id();
        world.insert_resource(session_with(SpawnPolicy::Scored(SpawnWeights::default())));
        let preview = spawn_preview(&mut world, player);
        let entities: Vec<Entity> = preview.rows.iter().map(|r| r.entity).collect();
        assert_eq!(entities, vec![near, far, disabled]);
        assert_eq!(preview.pick, Some(far));
        assert_eq!(preview.rows[0].terms.nearest_enemy_cm, 100);
        assert_eq!(
            preview.rows[2].terms.refused,
            Some(crate::rules::spawn::SpawnRefusal::Disabled)
        );
        // The preview does not move the session's state; the pick does.
        let before = world.resource::<MatchRules>().session.spawn.clone();
        assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(far));
        assert_ne!(world.resource::<MatchRules>().session.spawn, before);
    }

    /// ADR-8731 (spawn-selection review, logic-nodes union): a start the logic graph disabled
    /// (`PlayerStartDisabled`) is refused as `disabled` by every policy, the preview still lists it,
    /// and removing the marker makes it eligible again.
    #[test]
    fn a_player_start_disabled_start_is_refused_by_choose_spawn() {
        let mut world = World::new();
        let first = start(&mut world, Vec3::new(1.0, 0.0, 0.0), None);
        let second = start(&mut world, Vec3::new(2.0, 0.0, 0.0), None);
        world.entity_mut(first).insert(PlayerStartDisabled);
        let player = world.spawn_empty().id();
        // No session: S-14's first start in spawn order skips the disabled one.
        assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(second));
        for policy in [
            SpawnPolicy::First,
            SpawnPolicy::RoundRobin,
            SpawnPolicy::Scored(SpawnWeights::default()),
        ] {
            world.insert_resource(session_with(policy));
            for _ in 0..3 {
                assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(second));
            }
        }
        let preview = spawn_preview(&mut world, player);
        let entities: Vec<Entity> = preview.rows.iter().map(|r| r.entity).collect();
        assert_eq!(entities, vec![first, second]);
        assert_eq!(
            preview.rows[0].terms.refused,
            Some(crate::rules::spawn::SpawnRefusal::Disabled)
        );
        // Every start disabled: nothing is eligible (the caller falls back, as
        // `default_spawn_point` does).
        world.entity_mut(second).insert(PlayerStartDisabled);
        assert!(choose_spawn(&mut world, player).is_none());
        // Enabled again.
        world.entity_mut(first).remove::<PlayerStartDisabled>();
        assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(first));
    }

    #[test]
    fn ecs_block_test_without_physics_skips_an_occupied_start() {
        let mut world = World::new();
        start(&mut world, Vec3::new(0.0, 0.0, 0.0), None);
        let free = start(&mut world, Vec3::new(5.0, 0.0, 0.0), None);
        world.spawn((Transform::from_translation(Vec3::new(0.2, 0.0, 0.0)), TeamId(1)));
        let player = world.spawn_empty().id();
        world.insert_resource(SpawnShape {
            block_test: true,
            ..SpawnShape::default()
        });
        assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(free));
    }

    fn entity_index(entity: Entity) -> u32 {
        entity.index().index()
    }

    /// Review proof (rules-spawn-selection-review). [`choose_spawn`] documents spawn order, but
    /// [`crate::player::PlayerPlugin`] never installs `SpawnOrder<PlayerStart>`, so the gather
    /// sorts by entity index. Bevy holds freed ids on a 128-deep local list and then reuses
    /// them, so a start that arrives later can take a lower index than one that arrived first.
    /// The pick must follow arrival, which is the earlier start.
    #[test]
    fn proof_review_choose_spawn_follows_arrival_not_a_recycled_index() {
        let mut world = World::new();
        let anchor = world.spawn_empty().id();
        let early = start(&mut world, Vec3::new(1.0, 0.0, 0.0), None);
        assert!(entity_index(early) > entity_index(anchor));
        assert!(world.despawn(anchor));
        // One past the allocator's local free list, so the low index becomes reusable.
        for _ in 0..128 {
            let filler = world.spawn_empty().id();
            assert!(world.despawn(filler));
        }
        let mut late = None;
        for _ in 0..160 {
            let spawned = start(&mut world, Vec3::new(9.0, 0.0, 0.0), None);
            if entity_index(spawned) < entity_index(early) {
                late = Some(spawned);
                break;
            }
        }
        let late = late.expect("a later PlayerStart recycled an index below the earlier start");
        let player = world.spawn_empty().id();
        let pick = choose_spawn(&mut world, player).expect("a start");
        assert_eq!(
            pick.entity,
            early,
            "arrival order is the start at index {}; index order picked the recycled start at index {}",
            entity_index(early),
            entity_index(late)
        );
    }

    /// Spawn a start, despawn an anchor that held a lower index, flush Bevy's local free list,
    /// then spawn starts until one reuses an index below `first`: returns that later start.
    fn later_start_on_a_recycled_index(world: &mut World, first: Entity, anchor: Entity, at: Vec3) -> Entity {
        assert!(world.despawn(anchor));
        for _ in 0..128 {
            let filler = world.spawn_empty().id();
            assert!(world.despawn(filler));
        }
        for _ in 0..160 {
            let spawned = start(world, at, None);
            if entity_index(spawned) < entity_index(first) {
                return spawned;
            }
        }
        panic!("no later PlayerStart recycled an index below the earlier start");
    }

    /// ADR-8704: `PlayerPlugin` installs `SpawnOrder<PlayerStart>`, and with it the earlier start
    /// wins even when it lies farther along x than a later start on a recycled lower index (so
    /// neither the index nor the position order would pick it). `default_spawn_point` agrees.
    #[test]
    fn player_plugin_tracks_arrival_and_the_earlier_start_wins() {
        let mut app = App::new();
        app.add_plugins(crate::player::PlayerPlugin);
        let world = app.world_mut();
        assert!(
            starts_in_arrival_order(world),
            "PlayerPlugin installs the tracker"
        );
        let anchor = world.spawn_empty().id();
        let early = start(world, Vec3::new(9.0, 0.0, 0.0), None);
        let late = later_start_on_a_recycled_index(world, early, anchor, Vec3::new(1.0, 0.0, 0.0));
        assert!(entity_index(late) < entity_index(early));
        let player = world.spawn_empty().id();
        assert_eq!(choose_spawn(world, player).map(|p| p.entity), Some(early));
        assert_eq!(
            crate::rules::default_spawn_point(world, player),
            Vec3::new(9.0, 0.0, 0.0)
        );
        let preview = spawn_preview(world, player);
        assert!(preview.arrival_order);
        // The helper may leave extra starts that did not recycle; they arrived later still.
        let walk: Vec<Entity> = preview
            .rows
            .iter()
            .map(|r| r.entity)
            .filter(|e| [early, late].contains(e))
            .collect();
        assert_eq!(walk, vec![early, late]);
    }

    /// ADR-8704: with no tracker the documented order is position (x, then y, then z), not the
    /// entity index: a start on a recycled lower index but farther along x stays behind.
    #[test]
    fn without_a_tracker_starts_walk_in_position_order_not_index_order() {
        let mut world = World::new();
        assert!(!starts_in_arrival_order(&world));
        let anchor = world.spawn_empty().id();
        let near = start(&mut world, Vec3::new(2.0, 5.0, 0.0), None);
        let far = later_start_on_a_recycled_index(&mut world, near, anchor, Vec3::new(3.0, 0.0, 0.0));
        assert!(entity_index(far) < entity_index(near));
        // Same x, lower y: walked before `near`.
        let below = start(&mut world, Vec3::new(2.0, -1.0, 0.0), None);
        let player = world.spawn_empty().id();
        let preview = spawn_preview(&mut world, player);
        assert!(!preview.arrival_order);
        // Extra starts the helper left at `far`'s position sort beside it, after `near`.
        let walk: Vec<Entity> = preview
            .rows
            .iter()
            .map(|r| r.entity)
            .filter(|e| [below, near, far].contains(e))
            .collect();
        assert_eq!(walk, vec![below, near, far]);
        assert_eq!(choose_spawn(&mut world, player).map(|p| p.entity), Some(below));
        assert_eq!(
            crate::rules::default_spawn_point(&mut world, player),
            Vec3::new(2.0, -1.0, 0.0)
        );
    }

    /// The position key is IEEE total order: negative before positive, -0.0 before 0.0.
    #[test]
    fn start_key_orders_positions_by_total_order() {
        let keys: Vec<u32> = [-5.0f32, -0.0, 0.0, 1.0, f32::INFINITY]
            .into_iter()
            .map(total_order_bits)
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
    }
}
