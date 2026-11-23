// SPDX-License-Identifier: MPL-2.0
//! Sessions reach the game rules (spec 77 § 9, ADR-2413): in-process server and loopback clients
//! over the in-memory link, a manual session clock, no sockets.
//!
//! - `GameRules::on_player_joined` and `on_player_left` fire exactly once per remote player, with
//!   that player's `NetPlayer` entity, across a travel and a reconnect resume.
//! - A `Gone` peer's owned entities go to a live named successor, else to the server, or are
//!   despawned when the session rules say so; the character the game rules spawned for the
//!   player is despawned with it unless the rules keep it (KI-GP-033).
//! - Per-player ping and loss come from the loopback meters.

use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use regia_framework::net_session_bridge::NetRulesJoined;
use regia_framework::rules::{GameRules, GameRulesResource, RulesCtx, RulesPlugin, SpawnRequest};
use regia_net::NetSessionPlugin;
use regia_net::SessionAppExt;
use regia_net::model::{NetOwner, NetPlayer, NetPlayerId, PeerId};
use regia_net::session::{
    DisconnectReason, HostOptions, NetDespawnWithOwner, NetHandOffs, NetPlayerStats, NetSession,
    NetSuccessor, OwnedHandOff, SessionConditioner, SessionRules, ShaperSettings, SimInput,
};

type Log = Arc<Mutex<Vec<(&'static str, Entity, Option<NetPlayerId>)>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Rules that log every join and leave with the entity and the `NetPlayer` id on it.
struct Recording(Log);

impl GameRules for Recording {
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
        let id = ctx.world.get::<NetPlayer>(player).map(|p| p.id);
        lock(&self.0).push(("joined", player, id));
        Some(SpawnRequest::default())
    }

    fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
        let id = ctx.world.get::<NetPlayer>(player).map(|p| p.id);
        lock(&self.0).push(("left", player, id));
    }
}

fn player(peer: u32) -> NetPlayerId {
    NetPlayerId {
        peer: PeerId(peer),
        local: 0,
    }
}

fn server_app(log: &Log, clients: usize) -> App {
    server_app_with(log, clients, None)
}

fn server_app_with(log: &Log, clients: usize, conditioner: Option<SessionConditioner>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((NetSessionPlugin, RulesPlugin));
    app.insert_resource(GameRulesResource {
        rules: Box::new(Recording(log.clone())),
    });
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        s.set_manual_clock(0);
        if let Some(c) = conditioner {
            s.set_conditioner(c);
        }
        s.host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
        s.add_simulated_clients(clients, SimInput::Idle)
            .expect("simulated clients");
    }
    app
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
    }
}

fn entity_of(app: &App, p: NetPlayerId) -> Entity {
    app.world()
        .resource::<NetSession>()
        .player_entity(p)
        .expect("player entity")
}

fn count(log: &Log, kind: &str, e: Entity) -> usize {
    lock(log)
        .iter()
        .filter(|(k, ent, _)| *k == kind && *ent == e)
        .count()
}

#[test]
fn joined_and_left_fire_exactly_once_with_the_players_entity() {
    let log: Log = Arc::default();
    let mut app = server_app(&log, 2);
    frames(&mut app, 30);
    let (a, b) = (entity_of(&app, player(1)), entity_of(&app, player(2)));
    let mut joined = lock(&log).clone();
    joined.sort_by_key(|(_, _, id)| *id);
    assert_eq!(
        joined,
        vec![("joined", a, Some(player(1))), ("joined", b, Some(player(2)))],
        "one join per remote player, with its NetPlayer entity"
    );
    // The spawn request became a character owned by the player's peer.
    let pawn = app
        .world()
        .get::<NetRulesJoined>(a)
        .and_then(|j| j.pawn)
        .expect("pawn spawned for the rules' request");
    assert_eq!(app.world().get::<NetOwner>(pawn), Some(&NetOwner(PeerId(1))));

    // A travel brings every peer back to InGame: no second join.
    app.world_mut()
        .resource_mut::<NetSession>()
        .travel("levels/b.level.ron")
        .expect("travel");
    frames(&mut app, 30);
    // A reconnect resume reaches InGame again too: still no second join, and no leave.
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        let now = s.now_ms();
        if let Some(sims) = s.simulated_mut() {
            sims.client_mut(0)
                .core
                .disconnect(DisconnectReason::Suspended, now);
        }
    }
    frames(&mut app, 10);
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        let now = s.now_ms();
        if let Some(sims) = s.simulated_mut() {
            sims.client_mut(0).reconnect(now, 1);
        }
    }
    frames(&mut app, 30);
    assert_eq!(entity_of(&app, player(1)), a, "the resume keeps the entity");
    assert_eq!(count(&log, "joined", a), 1);
    assert_eq!(count(&log, "joined", b), 1);
    assert_eq!(lock(&log).iter().filter(|(k, _, _)| *k == "left").count(), 0);

    // Kick one: one leave, with its entity, while the entity still exists; then it is gone.
    app.world_mut()
        .resource_mut::<NetSession>()
        .kick(player(2), "bye")
        .expect("kick");
    frames(&mut app, 5);
    assert_eq!(count(&log, "left", b), 1);
    assert_eq!(
        lock(&log).last().copied(),
        Some(("left", b, Some(player(2)))),
        "the hook saw the NetPlayer component, so it ran before the despawn"
    );
    assert!(app.world().get_entity(b).is_err());
    // The other leaves by quitting: one leave for it too, and never a second one for either.
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        let now = s.now_ms();
        if let Some(sims) = s.simulated_mut() {
            sims.client_mut(0)
                .core
                .disconnect(DisconnectReason::ClientQuit, now);
        }
    }
    frames(&mut app, 30);
    assert_eq!(count(&log, "left", a), 1);
    assert_eq!(count(&log, "left", b), 1);
    let all = lock(&log).clone();
    assert_eq!(all.len(), 4, "{all:?}");
}

/// Session rules that despawn entities carrying [`Doomed`] and leave the rest to the default rule.
struct DespawnDoomed;

#[derive(Component)]
struct Doomed;

impl SessionRules for DespawnDoomed {
    fn owned_hand_off(
        &mut self,
        world: &World,
        entity: Entity,
        _peer: PeerId,
        live: &dyn Fn(PeerId) -> bool,
    ) -> OwnedHandOff {
        if world.get::<Doomed>(entity).is_some() {
            OwnedHandOff::Despawn
        } else {
            regia_net::session::players::default_hand_off(world, entity, live)
        }
    }
}

#[test]
fn a_gone_peers_entities_go_to_the_successor_the_server_or_away() {
    let log: Log = Arc::default();
    let mut app = server_app(&log, 2);
    app.set_session_rules(DespawnDoomed);
    frames(&mut app, 30);
    let gone = entity_of(&app, player(1));
    let pawn = app
        .world()
        .get::<NetRulesJoined>(gone)
        .and_then(|j| j.pawn)
        .expect("pawn");
    let world = app.world_mut();
    let to_successor = world.spawn((NetOwner(PeerId(1)), NetSuccessor(PeerId(2)))).id();
    let to_server = world.spawn(NetOwner(PeerId(1))).id();
    let dead_successor = world.spawn((NetOwner(PeerId(1)), NetSuccessor(PeerId(77)))).id();
    let doomed = world.spawn((NetOwner(PeerId(1)), Doomed)).id();
    let not_theirs = world.spawn(NetOwner(PeerId(2))).id();
    app.world_mut()
        .resource_mut::<NetSession>()
        .kick(player(1), "bye")
        .expect("kick");
    frames(&mut app, 5);
    let w = app.world();
    assert_eq!(w.get::<NetOwner>(to_successor), Some(&NetOwner(PeerId(2))));
    assert!(w.get::<NetOwner>(to_server).is_none(), "server-owned now");
    assert!(
        w.get::<NetOwner>(dead_successor).is_none(),
        "an unknown successor: the server"
    );
    assert!(w.get_entity(doomed).is_err(), "the session rules despawned it");
    assert_eq!(w.get::<NetOwner>(not_theirs), Some(&NetOwner(PeerId(2))));
    // KI-GP-033 (ADR-10070): the character the join spawned leaves with its player.
    assert!(
        w.get_entity(pawn).is_err(),
        "the player's character is despawned with the player"
    );
    let handoffs = w.resource::<NetHandOffs>();
    assert_eq!(handoffs.total, 5, "{:?}", handoffs.records);
    assert!(handoffs.records.iter().all(|r| r.from == PeerId(1)));
    assert!(
        handoffs
            .records
            .iter()
            .any(|r| r.entity == pawn.to_bits() && r.to == OwnedHandOff::Despawn),
        "the character's hand-off is a despawn: {:?}",
        handoffs.records
    );
    assert_eq!(count(&log, "left", gone), 1);
}

/// Rules that keep a leaver's character: `on_player_left` lifts `NetDespawnWithOwner` from the
/// characters the player's peer owns (KI-GP-033, ADR-10070).
struct KeepCharacters(Log);

impl GameRules for KeepCharacters {
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
        Recording(self.0.clone()).on_player_joined(ctx, player)
    }

    fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
        Recording(self.0.clone()).on_player_left(ctx, player);
        let Some(peer) = ctx.world.get::<NetPlayer>(player).map(|p| p.id.peer) else {
            return;
        };
        let mut q = ctx
            .world
            .query_filtered::<(Entity, &NetOwner), With<NetDespawnWithOwner>>();
        let theirs: Vec<Entity> = q
            .iter(ctx.world)
            .filter(|(_, o)| o.0 == peer)
            .map(|(e, _)| e)
            .collect();
        for e in theirs {
            ctx.world.entity_mut(e).remove::<NetDespawnWithOwner>();
        }
    }
}

/// KI-GP-033 (ADR-10070): leaving despawns the character the game rules spawned for the player,
/// for a kick and for a quit, with the default rules hand-off; rules that lift the marker in
/// `on_player_left` keep it, server-owned.
#[test]
fn a_leaving_players_character_is_despawned_unless_the_rules_keep_it() {
    let log: Log = Arc::default();
    let mut app = server_app(&log, 2);
    frames(&mut app, 30);
    let pawn_of = |app: &App, p: NetPlayerId| {
        let e = entity_of(app, p);
        app.world()
            .get::<NetRulesJoined>(e)
            .and_then(|j| j.pawn)
            .expect("the join spawned a character")
    };
    let kicked = pawn_of(&app, player(1));
    let quitter = pawn_of(&app, player(2));
    assert!(app.world().get::<NetDespawnWithOwner>(kicked).is_some());
    app.world_mut()
        .resource_mut::<NetSession>()
        .kick(player(1), "bye")
        .expect("kick");
    frames(&mut app, 5);
    assert!(
        app.world().get_entity(kicked).is_err(),
        "a kicked player's character is gone"
    );
    assert!(app.world().get_entity(quitter).is_ok(), "the other one stays");
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        let now = s.now_ms();
        if let Some(sims) = s.simulated_mut() {
            // The kick removed the first client: the quitter is the only one left.
            sims.client_mut(0)
                .core
                .disconnect(DisconnectReason::ClientQuit, now);
        }
    }
    frames(&mut app, 30);
    assert!(
        app.world().get_entity(quitter).is_err(),
        "a player who quits takes the character along"
    );

    // Rules that keep the character.
    let log: Log = Arc::default();
    let mut app = server_app(&log, 1);
    app.insert_resource(GameRulesResource {
        rules: Box::new(KeepCharacters(log.clone())),
    });
    frames(&mut app, 30);
    let kept = pawn_of(&app, player(1));
    app.world_mut()
        .resource_mut::<NetSession>()
        .kick(player(1), "bye")
        .expect("kick");
    frames(&mut app, 5);
    let w = app.world();
    assert!(
        w.get_entity(kept).is_ok() && w.get::<NetOwner>(kept).is_none(),
        "kept by the rules: the character stays, server-owned"
    );
    assert_eq!(count(&log, "left", entity_of_gone(&log)), 1);
}

/// The entity of the only player that left in `log`.
fn entity_of_gone(log: &Log) -> Entity {
    lock(log)
        .iter()
        .find(|(k, _, _)| *k == "left")
        .map(|(_, e, _)| *e)
        .expect("a leave")
}

#[test]
fn per_player_ping_and_loss_come_from_the_link_meters() {
    let log: Log = Arc::default();
    let shaped = ShaperSettings {
        latency_ms: 40,
        ..ShaperSettings::default()
    };
    let mut app = server_app_with(
        &log,
        2,
        Some(SessionConditioner {
            outbound: shaped,
            inbound: shaped,
            seed: 7,
        }),
    );
    frames(&mut app, 120);
    let stats = app.world().resource::<NetPlayerStats>();
    assert_eq!(stats.players.len(), 2, "{stats:?}");
    for p in [player(1), player(2)] {
        let row = stats.get(p).expect("row");
        assert!(row.ping_ms >= 60.0, "ping from the shaped delay: {row:?}");
        assert!(row.simulated);
        assert_eq!(row.loss_from_player, Some(0.0));
    }
    assert!(stats.server.is_none(), "a server has no server row");
}
