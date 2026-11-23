// SPDX-License-Identifier: MPL-2.0
//! Behaviour-tree, perception and navigation adapter for `regia_framework::net_bots` (ADR-3346).
//!
//! This file lives in `regia_ai` because the crate already depends on `regia_framework`; putting
//! these types in the framework would create a dependency cycle. The framework owns connection
//! and reconnect policy, while [`ServerBotTask`] turns a [`NetBotBrain`] into `CharacterInput` on
//! each fixed tick. No connection, command packet or client prediction stream is created.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{Character, CharacterInput};
use regia_framework::net_bots::{NetBotBrain, NetBotDriven, net_bot_policy};
use regia_nav::agent::DEFAULT_PATH_EXTENTS;
use regia_nav::{NavAgentState, NavMesh, NavObstacles, NavQuery};
use regia_net::NetSet;

use crate::perception::{Perception, in_sight_cone, line_of_sight};
use crate::task::{BtContext, BtStatus, BtTask, BtTaskRegistration};
use crate::tree::{BehaviorTree, BehaviorTreeDef, BtTaskRegistry};
use crate::{AiPlugin, AiStep, Blackboard};

/// Installs the built-in server-bot leaf and attaches it to every new [`NetBotBrain`].
///
/// Add this after `NetBotsPlugin`. `AiPlugin` is added when absent. The set ordering is explicit:
/// session/apply → AI input → predicted character step → replication gather/send.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServerBotAiPlugin;

impl Plugin for ServerBotAiPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<AiPlugin>()) {
            app.add_plugins(AiPlugin);
        }
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<ServerBotTask>();
        // Perception ties follow the character's arrival, not its entity id (ADR-1347).
        regia_ecs::spawn_order::track_spawn_order::<Character>(app.world_mut());
        app.configure_sets(FixedUpdate, AiStep.after(NetSet::Apply).before(NetSet::Predict))
            .add_systems(PreUpdate, attach_bot_trees.after(net_bot_policy))
            .add_systems(Update, remove_bot_trees);
    }
}

fn attach_bot_trees(world: &mut World) {
    let mut query = world.query_filtered::<(Entity, &NetBotBrain), Added<NetBotBrain>>();
    let brains: Vec<(Entity, u32, f32)> = query
        .iter(world)
        .map(|(entity, brain)| (entity, brain.bot, brain.difficulty.perception_radius))
        .collect();
    for (entity, bot, radius) in brains {
        let task_path = <ServerBotTask as BtTaskRegistration>::task_type_path();
        let compiled = {
            let registry = world.resource::<BtTaskRegistry>();
            BehaviorTree::compile(&BehaviorTreeDef::leaf("server_bot", task_path), registry)
        };
        let Ok(tree) = compiled else {
            tracing::error!(target: "regia_ai", bot, "server bot behaviour tree failed to compile");
            continue;
        };
        world.entity_mut(entity).insert((
            tree,
            Blackboard::default(),
            Perception {
                sight_radius: radius,
                sight_half_angle: std::f32::consts::PI,
                ..Perception::default()
            },
        ));
    }
}

fn remove_bot_trees(mut removed: RemovedComponents<NetBotDriven>, mut commands: Commands) {
    for entity in removed.read() {
        if let Ok(mut entity) = commands.get_entity(entity) {
            entity.remove::<(BehaviorTree, crate::BehaviorTreeStatus, Blackboard, Perception)>();
        }
    }
}

/// SplitMix64's public-domain mixing function, used only to derive seeded noise.
fn next_noise(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    // The high 24 bits map exactly to [0, 1); centre and double for [-1, 1).
    let unit = (z >> 40) as f32 * (1.0 / 16_777_216.0);
    unit * 2.0 - 1.0
}

fn patrol_destination(brain: &mut NetBotBrain, position: Vec3) -> Vec3 {
    let x = next_noise(&mut brain.noise_state) * 20.0;
    let z = next_noise(&mut brain.noise_state) * 20.0;
    Vec3::new(position.x + x, position.y, position.z + z)
}

fn nearest_perceived(
    world: &mut World,
    entity: Entity,
    position: Vec3,
    forward: Vec3,
    radius: f32,
) -> Option<(Entity, Vec3)> {
    let mut query = world.query_filtered::<Entity, With<Character>>();
    let others: Vec<Entity> = query.iter(world).filter(|other| *other != entity).collect();
    let mut candidates: Vec<(Entity, Vec3, f32, regia_ecs::spawn_order::SpawnKey)> = Vec::new();
    for other in others {
        // Read the pose after the character query ends. `world_pose` walks `ChildOf`, which
        // aliases a query that is still iterating.
        let Some((target, _)) = crate::tick_budget::world_pose(world, other) else {
            continue;
        };
        if !target.is_finite() {
            continue;
        }
        let distance = (target - position).length_squared();
        // Both poses are finite, so this is the radius test without a negated partial compare.
        if distance > radius * radius {
            continue;
        }
        if !in_sight_cone(position, forward, target, std::f32::consts::PI, radius) {
            continue;
        }
        if line_of_sight(world, position, target, &[entity, other]).is_some() {
            continue;
        }
        let key = world
            .get_resource::<regia_ecs::spawn_order::SpawnOrder<Character>>()
            .map(|order| order.key(other))
            .unwrap_or((u64::MAX, other.index().index(), other.to_bits()));
        candidates.push((other, target, distance, key));
    }
    candidates.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.3.cmp(&b.3)));
    candidates
        .first()
        .map(|(entity, position, _, _)| (*entity, *position))
}

fn follow_path(world: &mut World, entity: Entity, position: Vec3) -> CharacterInput {
    let Some(mut nav) = world.get_mut::<NavAgentState>(entity) else {
        return CharacterInput::default();
    };
    if let Some(waypoint) = nav.current_waypoint() {
        let delta = waypoint - position;
        if Vec3::new(delta.x, 0.0, delta.z).length_squared() <= 0.35 * 0.35 {
            nav.next_waypoint = nav.next_waypoint.saturating_add(1);
        }
    }
    let direction = nav
        .current_waypoint()
        .map_or(Vec3::ZERO, |waypoint| waypoint - position);
    CharacterInput {
        move_dir: Vec3::new(direction.x, 0.0, direction.z).clamp_length_max(1.0),
        jump: false,
        crouch: false,
        sprint: false,
    }
}

/// The built-in leaf: perceive, plan a nav path and produce one complete character input.
#[derive(Default)]
pub struct ServerBotTask;

impl BtTaskRegistration for ServerBotTask {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self)
    }

    fn task_type_path() -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl BtTask for ServerBotTask {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let _span = tracing::trace_span!(target: "regia_ai", "net_bot_tick").entered();
        let Some(mut brain) = ctx.world.get::<NetBotBrain>(ctx.entity).cloned() else {
            return BtStatus::Failure;
        };
        // The local chain, not `BtContext::position`: that reads `GlobalTransform`, which is one
        // frame behind on a later fixed step and would tie the plan to the frame rate.
        let Some((position, forward)) = crate::tick_budget::world_pose(ctx.world, ctx.entity) else {
            return BtStatus::Failure;
        };
        if !position.is_finite() || !forward.is_finite() {
            return BtStatus::Failure;
        }
        let forward = forward.normalize_or_zero();
        if brain.reaction_remaining > 0 {
            brain.reaction_remaining -= 1;
            // Reaction delay defers a new decision; it does not release the current path's input.
            if let Some(mut stored) = ctx.world.get_mut::<NetBotBrain>(ctx.entity) {
                *stored = brain;
            }
            return BtStatus::Running;
        }
        brain.difficulty = brain.difficulty.sanitized();
        if let Some(mut perception) = ctx.world.get_mut::<Perception>(ctx.entity) {
            perception.sight_radius = brain.difficulty.perception_radius;
        }
        let perceived = nearest_perceived(
            ctx.world,
            ctx.entity,
            position,
            forward,
            brain.difficulty.perception_radius,
        );
        brain.perceived_target = perceived.map(|(entity, _)| entity);
        let walking = ctx
            .world
            .get::<NavAgentState>(ctx.entity)
            .is_some_and(NavAgentState::has_path);
        let destination = match perceived {
            Some((_, p)) => p,
            // Finish the current leg (a patrol point, or where a lost target was last seen); a new
            // patrol point is drawn only when the path is done or gone. A fresh point every
            // decision would turn the patrol into a random walk that never arrives.
            None => match brain.planned_destination {
                Some(planned) if walking => planned,
                _ => patrol_destination(&mut brain, position),
            },
        };
        if let Some((_, target)) = perceived {
            let direction = target - position;
            if direction.is_finite() {
                // `det_atan2`, not platform `atan2`: the last bit differs between libm builds and
                // would split lockstep peers (aim yaw and pitch are deterministic state).
                let base_yaw = regia_core::math::det_atan2(direction.x, -direction.z);
                brain.aim_yaw =
                    base_yaw + next_noise(&mut brain.noise_state) * brain.difficulty.aim_error_radians;
                let horizontal = Vec3::new(direction.x, 0.0, direction.z).length();
                let base_pitch = regia_core::math::det_atan2(direction.y, horizontal);
                brain.aim_pitch =
                    base_pitch + next_noise(&mut brain.noise_state) * brain.difficulty.aim_error_radians;
            }
        }
        let needs_plan = brain
            .planned_destination
            .is_none_or(|old| old.distance_squared(destination) > 0.25)
            || ctx
                .world
                .get::<NavAgentState>(ctx.entity)
                .is_none_or(|nav| !nav.has_path());
        if needs_plan {
            let path = ctx.world.get_resource::<NavMesh>().and_then(|mesh| {
                let obstacles = ctx.world.get_resource::<NavObstacles>();
                NavQuery { mesh, obstacles }.find_path(position, destination, DEFAULT_PATH_EXTENTS)
            });
            if let Some(mut nav) = ctx.world.get_mut::<NavAgentState>(ctx.entity) {
                if let Some(path) = path {
                    nav.set_path(path);
                } else {
                    nav.clear_path();
                }
            }
            brain.planned_destination = Some(destination);
        }
        let input = follow_path(ctx.world, ctx.entity, position);
        if let Some(mut stored) = ctx.world.get_mut::<CharacterInput>(ctx.entity) {
            *stored = input;
        }
        brain.reaction_remaining = brain.difficulty.reaction_ticks;
        brain.decisions = brain.decisions.saturating_add(1);
        if let Some(mut blackboard) = ctx.blackboard_mut() {
            if let Some(target) = brain.perceived_target {
                blackboard.insert("perception_target", target);
                blackboard.insert("perception_target_position", destination);
            } else {
                blackboard.remove("perception_target");
                blackboard.remove("perception_target_position");
            }
            blackboard.insert("patrol_destination", destination);
        }
        if let Some(mut stored) = ctx.world.get_mut::<NetBotBrain>(ctx.entity) {
            *stored = brain;
        }
        BtStatus::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::character::{CharacterCollisionQuery, CharacterState, FlatPlaneQuery};
    use regia_framework::net_bots::{
        BotDifficulty, NetBotsConfig, NetBotsPlugin, add_bot, install_net_bots, net_bot_rows,
    };
    use regia_framework::net_character::{CharacterNetInput, install_net_character};
    use regia_framework::net_session_bridge::NetRulesJoined;
    use regia_framework::player::PlayerStart;
    use regia_framework::rules::{GameRules, GameRulesResource, RulesCtx, RulesPlugin, SpawnRequest};
    use regia_nav::NavMeshBuilder;
    use regia_net::runtime::NetNode;
    use regia_net::session::net_session::NetDropped;
    use regia_net::testing::{FrameFate, LoopbackHub};
    use regia_net::{NetId, NetMode, NetOwner, NetPlayer, NetPlayerId, PeerId};
    use regia_scene::Transform;
    use std::hash::{Hash, Hasher};

    fn app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, RulesPlugin));
        app.insert_resource(NetBotsConfig {
            min_players: 4,
            max_bots: 4,
            seed,
            difficulty: BotDifficulty {
                reaction_ticks: 0,
                aim_error_radians: 0.1,
                perception_radius: 30.0,
            },
            ..NetBotsConfig::default()
        });
        app.add_plugins((NetBotsPlugin, ServerBotAiPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.world_mut().spawn((Transform::default(), PlayerStart));
        let floor = [
            [
                Vec3::new(-64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, 64.0),
            ],
            [
                Vec3::new(-64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, 64.0),
                Vec3::new(-64.0, 0.0, 64.0),
            ],
        ];
        let mesh = NavMeshBuilder::default().build(&floor).expect("test navmesh");
        app.insert_resource(mesh);
        app
    }

    #[test]
    fn four_connectionless_bots_follow_nav_paths() {
        let mut app = app(17);
        app.update();
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.len(), 4);
        for row in rows {
            let pawn = row.pawn.expect("bot pawn");
            let nav = app.world().get::<NavAgentState>(pawn).expect("nav state");
            assert!(nav.has_path(), "bot {} has a nav path", row.id);
            let brain = app.world().get::<NetBotBrain>(pawn).expect("brain");
            assert!(brain.decisions > 0);
            assert!(app.world().get::<BehaviorTree>(pawn).is_some());
            assert!(app.world().get::<Perception>(pawn).is_some());
        }
    }

    type InputParts = (Vec3, bool, bool, bool);
    type BotSnapshot = (u64, u64, InputParts, Vec<Vec3>);

    fn input_parts(input: &CharacterInput) -> InputParts {
        (input.move_dir, input.jump, input.crouch, input.sprint)
    }

    #[test]
    fn one_seed_repeats_brain_and_input_state_exactly() {
        fn run() -> Vec<BotSnapshot> {
            let mut app = app(91);
            for _ in 0..8 {
                app.update();
            }
            net_bot_rows(app.world_mut())
                .into_iter()
                .map(|row| {
                    let pawn = row.pawn.expect("pawn");
                    let brain = app.world().get::<NetBotBrain>(pawn).expect("brain");
                    let input = app.world().get::<CharacterInput>(pawn).cloned().expect("input");
                    let nav = app.world().get::<NavAgentState>(pawn).expect("nav");
                    (
                        brain.noise_state,
                        brain.decisions,
                        input_parts(&input),
                        nav.waypoints.clone(),
                    )
                })
                .collect()
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn reaction_delay_and_seeded_aim_error_are_deterministic() {
        let mut app = app(4);
        let id = add_bot(
            app.world_mut(),
            BotDifficulty {
                reaction_ticks: 3,
                aim_error_radians: 0.5,
                perception_radius: 20.0,
            },
        )
        .expect("extra bot");
        let pawn = net_bot_rows(app.world_mut())
            .into_iter()
            .find(|row| row.id == id)
            .and_then(|row| row.pawn)
            .expect("pawn");
        app.update();
        assert_eq!(
            app.world().get::<NetBotBrain>(pawn).map(|b| b.reaction_remaining),
            Some(2)
        );
        assert_eq!(
            app.world().get::<CharacterInput>(pawn).map(input_parts),
            Some(input_parts(&CharacterInput::default()))
        );
        for _ in 0..3 {
            app.update();
        }
        assert!(
            app.world()
                .get::<NetBotBrain>(pawn)
                .is_some_and(|b| b.decisions > 0)
        );
    }

    #[test]
    fn reaction_delay_preserves_the_current_path_input() {
        let mut app = app(23);
        app.update();
        let rows = net_bot_rows(app.world_mut());
        let pawn = rows[0].pawn.expect("pawn");
        for row in rows.iter().skip(1) {
            if let Some(other) = row.pawn {
                app.world_mut().despawn(other);
            }
        }
        let before = app.world().get::<CharacterInput>(pawn).cloned().expect("input");
        assert!(before.move_dir.length_squared() > 0.0);
        {
            let mut brain = app.world_mut().get_mut::<NetBotBrain>(pawn).expect("brain");
            brain.difficulty.reaction_ticks = 2;
            brain.reaction_remaining = 2;
        }
        app.update();
        let after = app.world().get::<CharacterInput>(pawn).expect("input");
        assert_eq!(
            input_parts(after),
            input_parts(&before),
            "a delayed decision keeps following the old path"
        );
        assert_eq!(
            app.world().get::<NetBotBrain>(pawn).map(|b| b.reaction_remaining),
            Some(1)
        );
    }

    fn test_mesh() -> NavMesh {
        let floor = [
            [
                Vec3::new(-64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, 64.0),
            ],
            [
                Vec3::new(-64.0, 0.0, -64.0),
                Vec3::new(64.0, 0.0, 64.0),
                Vec3::new(-64.0, 0.0, 64.0),
            ],
        ];
        NavMeshBuilder::default().build(&floor).expect("test navmesh")
    }

    struct SpreadRules;

    impl GameRules for SpreadRules {
        fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
            let index = ctx.world.get::<regia_framework::player::Player>(player)?.index;
            Some(SpawnRequest {
                transform: Transform::from_translation(Vec3::new(f32::from(index) * 5.0, 1.2, 8.0)),
                ..SpawnRequest::default()
            })
        }
    }

    fn loopback_setup(world: &mut World) {
        world.insert_resource(GameRulesResource {
            rules: Box::new(SpreadRules),
        });
        world.insert_resource(NetBotsConfig {
            min_players: 4,
            max_bots: 4,
            join_delay_ticks: 2,
            seed: 91,
            difficulty: BotDifficulty {
                reaction_ticks: 0,
                aim_error_radians: 0.1,
                perception_radius: 30.0,
            },
            ..NetBotsConfig::default()
        });
        install_net_bots(world);
        world.insert_resource(CharacterCollisionQuery {
            query: Box::new(FlatPlaneQuery),
        });
        install_net_character::<CharacterNetInput>(world).expect("net character");
        world.init_resource::<BtTaskRegistry>();
        world.resource_mut::<BtTaskRegistry>().register::<ServerBotTask>();
        world.insert_resource(test_mesh());
        world.spawn((Transform::default(), PlayerStart));
    }

    fn drive_loopback_trees(world: &mut World) {
        attach_bot_trees(world);
        let mut query = world.query_filtered::<Entity, With<NetBotBrain>>();
        let mut entities: Vec<Entity> = query.iter(world).collect();
        entities.sort_by_key(|entity| entity.to_bits());
        for entity in entities {
            let Some(mut tree) = world
                .get_entity_mut(entity)
                .ok()
                .and_then(|mut e| e.take::<BehaviorTree>())
            else {
                continue;
            };
            let mut context = BtContext {
                world,
                entity,
                dt: 1.0 / 60.0,
            };
            tree.tick(&mut context);
            world.entity_mut(entity).insert(tree);
        }
    }

    fn loopback() -> LoopbackHub {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, loopback_setup);
        hub.server
            .gameplay
            .add_systems((net_bot_policy, drive_loopback_trees).chain());
        hub
    }

    #[test]
    fn loopback_four_bots_use_paths_and_allocate_no_peer_or_send_bytes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut hub = loopback();
        let send_bytes = Arc::new(AtomicUsize::new(0));
        let counted = send_bytes.clone();
        hub.down_filter = Some(Box::new(move |datagram, _| {
            counted.fetch_add(datagram.bytes.len(), Ordering::Relaxed);
            FrameFate::Deliver
        }));
        hub.run(4);
        assert_eq!(net_bot_rows(&mut hub.server.world).len(), 4);
        let mut moving = 0;
        for row in net_bot_rows(&mut hub.server.world) {
            let pawn = row.pawn.expect("pawn");
            let nav = hub.server.world.get::<NavAgentState>(pawn).expect("nav state");
            assert!(nav.target.is_some(), "bot {} planned a nav destination", row.id);
            assert!(nav.waypoints.len() >= 2, "bot {} received a nav corridor", row.id);
            if hub
                .server
                .world
                .get::<CharacterInput>(pawn)
                .is_some_and(|input| input.move_dir.length_squared() > 0.0)
            {
                moving += 1;
            }
        }
        assert!(moving > 0, "spaced bots follow their nav corridors");
        assert!(hub.server.world.resource::<NetNode>().peers().is_empty());
        assert!(hub.clients.is_empty());
        // No bot peer exists, so there is no bot transport queue or datagram payload. Replicated
        // bot state is exercised below and is sent only to real clients.
        assert_eq!(hub.client_logs.len(), 0);
        assert_eq!(send_bytes.load(Ordering::Relaxed), 0);
    }

    fn replicated_hash(world: &mut World) -> u64 {
        let mut query = world.query::<(&NetId, &Transform, &CharacterState)>();
        let mut rows: Vec<_> = query
            .iter(world)
            .map(|(id, transform, state)| (*id, *transform, state.clone()))
            .collect();
        rows.sort_by_key(|(id, _, _)| *id);
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for (id, transform, state) in rows {
            id.hash(&mut hash);
            transform.translation.to_array().map(f32::to_bits).hash(&mut hash);
            transform.rotation.to_array().map(f32::to_bits).hash(&mut hash);
            state.velocity.to_array().map(f32::to_bits).hash(&mut hash);
            state.grounded.hash(&mut hash);
            state.time_in_air.to_bits().hash(&mut hash);
            format!("{:?}", state.mode).hash(&mut hash);
        }
        hash.finish()
    }

    #[test]
    fn loopback_one_seed_gives_identical_replicated_state_hashes() {
        let mut a = loopback();
        let mut b = loopback();
        for _ in 0..30 {
            a.step();
            b.step();
        }
        assert_eq!(
            replicated_hash(&mut a.server.world),
            replicated_hash(&mut b.server.world)
        );
        let client = a.add_client();
        a.run(60);
        let proxies: Vec<Entity> = {
            let mut query = a.clients[client]
                .world
                .query_filtered::<Entity, With<NetBotDriven>>();
            query.iter(&a.clients[client].world).collect()
        };
        // NetBotDriven is deliberately server-only; replicated characters arrive as ordinary proxies.
        assert!(proxies.is_empty());
        let replicated: usize = {
            let mut query = a.clients[client].world.query::<&NetId>();
            query.iter(&a.clients[client].world).count()
        };
        let server_ids = {
            let mut query = a.server.world.query::<&NetId>();
            query.iter(&a.server.world).count()
        };
        let proxy_count = {
            let mut query = a.clients[client].world.query::<&regia_net::NetRole>();
            query.iter(&a.clients[client].world).count()
        };
        let log_datagrams: usize = a.client_logs[client].len();
        // The client's first input creates NetPlayer; join_delay_ticks=2 then yields one bot.
        assert_eq!(net_bot_rows(&mut a.server.world).len(), 3);
        assert_eq!(server_ids, 3);
        assert_eq!(proxy_count, 3);
        assert_eq!(replicated, 3, "all remaining bot pawns reached the real client");
        assert!(a.clients[client].connected);
        assert!(log_datagrams > 0);
    }

    #[test]
    fn bot_input_uses_the_character_clamp_even_with_a_human_owner() {
        let mut hub = loopback();
        let peer = hub.add_client();
        let pawn = hub
            .server
            .world
            .spawn((
                regia_net::Replicated::default(),
                Transform::from_translation(Vec3::new(0.0, 1.2, 0.0)),
                regia_framework::net_character::NetCharacter::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                CharacterInput {
                    move_dir: Vec3::new(100.0, 0.0, 0.0),
                    ..CharacterInput::default()
                },
                NetOwner(PeerId(1)),
                NetBotDriven { bot: 999 },
            ))
            .id();
        hub.step();
        let state = hub.server.world.get::<CharacterState>(pawn).expect("state");
        assert!(
            state.velocity.x > 0.0,
            "NetBotDriven bypasses the missing remote command"
        );
        assert!(state.velocity.x <= 6.0, "the normal movement clamp still applies");
        assert!(hub.clients[peer].connected);
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
    struct TestScore(u32);

    #[test]
    fn loopback_drop_takeover_and_resume_preserve_pawn_and_score() {
        let mut hub = loopback();
        hub.add_client();
        let id = NetPlayerId {
            peer: PeerId(1),
            local: 0,
        };
        let pawn = hub
            .server
            .world
            .spawn((
                regia_net::Replicated::default(),
                Transform::from_translation(Vec3::new(4.0, 1.2, 2.0)),
                regia_framework::net_character::NetCharacter::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                NetOwner(id.peer),
                TestScore(41),
            ))
            .id();
        let player = hub
            .server
            .world
            .spawn((NetPlayer { id }, NetRulesJoined { pawn: Some(pawn) }, NetDropped))
            .id();
        hub.step();
        let bots = net_bot_rows(&mut hub.server.world);
        let takeover = bots
            .iter()
            .find(|bot| bot.takeover_of == Some(id))
            .expect("takeover");
        assert_eq!(takeover.pawn, Some(pawn));
        assert_eq!(hub.server.world.get::<TestScore>(pawn), Some(&TestScore(41)));
        assert_eq!(hub.server.world.get::<NetOwner>(pawn), Some(&NetOwner(id.peer)));
        hub.server.world.entity_mut(player).remove::<NetDropped>();
        hub.step();
        assert!(
            !net_bot_rows(&mut hub.server.world)
                .iter()
                .any(|bot| bot.takeover_of == Some(id))
        );
        assert_eq!(hub.server.world.get::<TestScore>(pawn), Some(&TestScore(41)));
        assert!(hub.server.world.get::<NetBotDriven>(pawn).is_none());
        assert_eq!(hub.server.world.get::<NetOwner>(pawn), Some(&NetOwner(id.peer)));
    }

    fn tick_bot(world: &mut World, entity: Entity) -> BtStatus {
        let mut task = ServerBotTask;
        let mut ctx = BtContext {
            world,
            entity,
            dt: 1.0 / 60.0,
        };
        task.tick(&mut ctx)
    }

    /// The leaf must plan from the local transform chain. A stale `GlobalTransform` is one frame
    /// behind and would move the patrol point with the frame rate.
    #[test]
    fn audfix_bot_plans_from_the_local_pose_not_a_stale_global() {
        let mut world = World::new();
        let entity = world
            .spawn((
                Transform::from_translation(Vec3::ZERO),
                regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    80.0, 0.0, 0.0,
                ))),
                NetBotBrain::new(
                    1,
                    BotDifficulty {
                        reaction_ticks: 0,
                        aim_error_radians: 0.0,
                        perception_radius: 0.0,
                    },
                    1,
                ),
                Blackboard::default(),
            ))
            .id();
        assert_eq!(tick_bot(&mut world, entity), BtStatus::Running);
        let planned = world
            .get::<NetBotBrain>(entity)
            .expect("brain")
            .planned_destination
            .expect("a patrol point");
        assert!(
            planned.x.abs() < 25.0,
            "planned from the stale global: {planned:?}"
        );
    }

    /// Aim yaw is documented as deterministic state. Platform `atan2` differs by one ulp from
    /// `det_atan2` at this input (kernel `0x3d8d_31c2`, glibc `0x3d8d_31c3`).
    #[test]
    fn audfix_bot_aim_yaw_uses_det_atan2_bits() {
        let mut world = World::new();
        let origin = Transform::from_translation(Vec3::ZERO);
        let entity = world
            .spawn((
                origin,
                regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::ZERO)),
                NetBotBrain::new(
                    1,
                    BotDifficulty {
                        reaction_ticks: 0,
                        aim_error_radians: 0.0,
                        perception_radius: 30.0,
                    },
                    1,
                ),
                Blackboard::default(),
            ))
            .id();
        let y = f32::from_bits(0x3d8d_6b23);
        world.spawn((Character, Transform::from_translation(Vec3::new(y, 0.0, -1.0))));
        assert_eq!(tick_bot(&mut world, entity), BtStatus::Running);
        let yaw = world.get::<NetBotBrain>(entity).expect("brain").aim_yaw;
        assert_eq!(yaw.to_bits(), 0x3d8d_31c2, "aim_yaw bits {:x}", yaw.to_bits());
    }

    /// Two characters at the same point: the earlier `Character` arrival wins, not the smaller
    /// `Entity::to_bits`. Fresh entity bits are not monotonic in spawn order.
    #[test]
    fn audfix_bot_perception_tie_follows_character_arrival() {
        let mut world = World::new();
        assert!(
            regia_ecs::spawn_order::track_spawn_order::<Character>(&mut world),
            "character spawn order must be tracking"
        );
        let entity = world
            .spawn((
                Transform::from_translation(Vec3::ZERO),
                NetBotBrain::new(
                    1,
                    BotDifficulty {
                        reaction_ticks: 0,
                        aim_error_radians: 0.0,
                        perception_radius: 30.0,
                    },
                    1,
                ),
                Blackboard::default(),
            ))
            .id();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        // The holder with the larger `to_bits` receives `Character` first, so arrival order
        // opposes the old ascending `to_bits` tie-break.
        let (earlier, later) = if first.to_bits() > second.to_bits() {
            (first, second)
        } else {
            (second, first)
        };
        assert!(
            earlier.to_bits() > later.to_bits(),
            "holders {first:?} bits {} and {second:?} bits {} do not differ",
            first.to_bits(),
            second.to_bits(),
        );
        let spot = Vec3::new(0.0, 0.0, -5.0);
        world
            .entity_mut(earlier)
            .insert((Character, Transform::from_translation(spot)));
        world
            .entity_mut(later)
            .insert((Character, Transform::from_translation(spot)));
        assert_eq!(tick_bot(&mut world, entity), BtStatus::Running);
        let target = world.get::<NetBotBrain>(entity).expect("brain").perceived_target;
        assert_eq!(
            target,
            Some(earlier),
            "earlier arrival {earlier:?} bits {} lost to later {later:?} bits {}",
            earlier.to_bits(),
            later.to_bits(),
        );
    }

    /// With nothing perceived, the bot walks its patrol leg to the end: the destination stays
    /// while the path lasts, even with no reaction delay (a decision every tick).
    #[test]
    fn a_patrol_leg_is_kept_while_its_path_lasts() {
        let mut world = World::new();
        let floor = [
            [
                Vec3::new(-40.0, 0.0, -40.0),
                Vec3::new(40.0, 0.0, -40.0),
                Vec3::new(40.0, 0.0, 40.0),
            ],
            [
                Vec3::new(-40.0, 0.0, -40.0),
                Vec3::new(40.0, 0.0, 40.0),
                Vec3::new(-40.0, 0.0, 40.0),
            ],
        ];
        let mesh = NavMeshBuilder::new(regia_nav::NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        })
        .build(&floor)
        .expect("the floor builds");
        world.insert_resource(mesh);
        let entity = world
            .spawn((
                Transform::from_translation(Vec3::ZERO),
                NetBotBrain::new(
                    7,
                    BotDifficulty {
                        reaction_ticks: 0,
                        aim_error_radians: 0.0,
                        perception_radius: 0.0,
                    },
                    1,
                ),
                NavAgentState::default(),
                Blackboard::default(),
            ))
            .id();
        assert_eq!(tick_bot(&mut world, entity), BtStatus::Running);
        let first = world
            .get::<NetBotBrain>(entity)
            .expect("brain")
            .planned_destination
            .expect("a patrol point");
        assert!(
            world.get::<NavAgentState>(entity).expect("nav").has_path(),
            "the patrol point {first:?} is on the floor, so there is a path"
        );
        for tick in 0..5 {
            assert_eq!(tick_bot(&mut world, entity), BtStatus::Running);
            let planned = world
                .get::<NetBotBrain>(entity)
                .expect("brain")
                .planned_destination
                .expect("a patrol point");
            assert_eq!(
                planned, first,
                "tick {tick}: the patrol leg was dropped for a new point"
            );
        }
    }

    /// A non-finite pose must not be written into the patrol destination.
    #[test]
    fn audfix_bot_rejects_a_non_finite_pose() {
        let mut world = World::new();
        let entity = world
            .spawn((
                Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)),
                NetBotBrain::new(
                    1,
                    BotDifficulty {
                        reaction_ticks: 0,
                        aim_error_radians: 0.0,
                        perception_radius: 0.0,
                    },
                    1,
                ),
                Blackboard::default(),
            ))
            .id();
        assert_eq!(tick_bot(&mut world, entity), BtStatus::Failure);
        let planned = world
            .get::<NetBotBrain>(entity)
            .expect("brain")
            .planned_destination;
        assert!(planned.is_none(), "poisoned destination {planned:?}");
    }
}
