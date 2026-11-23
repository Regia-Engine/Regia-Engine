// SPDX-License-Identifier: MPL-2.0
//! Fog of war over the network (audit gap FW-5): the fog relevance filter over the in-memory
//! `LoopbackHub` at fixed steps (no sockets, no wall clock). A dedicated server, clients on fog
//! teams; the client worlds are read after every step.

use std::collections::BTreeSet;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::fog_relevance::{
    FOG_RELEVANCE_PROFILE, FogEffect, FogEventPolicy, FogRelevance, FogRelevanceSettings, FogTeamOf, FogUnit,
    fog_event_shown, fog_relevance_prepare, install_fog_relevance,
};
use regia_framework::fog_view::{FogRules, SightClass};
use regia_framework::sim_bridge::EventPlace;
use regia_net::replication::register_replicated;
use regia_net::testing::LoopbackHub;
use regia_net::{
    NetId, NetInterest, NetMode, NetProfileRef, Replicate, ReplicateOptions, Replicated,
    TransformReplication, install_net_interest,
};
use regia_scene::transform::Transform;

/// The only replicated component: which test entity this is.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Tag {
    id: u32,
}

fn setup(world: &mut World) {
    register_replicated::<Tag>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        install_fog_relevance(
            world,
            FogRelevance::new(FogRules::default(), FogRelevanceSettings::default()),
        );
    }
}

/// No grace: hidden is hidden.
fn strict() -> FogRelevanceSettings {
    FogRelevanceSettings {
        linger_ticks: 0,
        lead_ticks: 0,
        ..FogRelevanceSettings::default()
    }
}

/// A dedicated server with `teams.len()` clients; client `i` is peer `i + 1` on fog team
/// `teams[i]` (`None`: no team).
fn hub(teams: &[Option<u32>], settings: FogRelevanceSettings) -> LoopbackHub {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
    hub.server.gameplay.add_systems(fog_relevance_prepare);
    hub.server
        .world
        .resource_mut::<FogRelevance>()
        .set_settings(settings);
    for team in teams {
        let client = hub.add_client();
        if let Some(team) = team {
            let peer = hub.clients[client].peer;
            hub.server.world.resource_mut::<FogTeamOf>().set(peer, *team);
        }
    }
    hub
}

fn at(x: f32) -> Transform {
    Transform {
        translation: Vec3::new(x, 0.0, 0.0),
        ..Transform::default()
    }
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn unit(hub: &mut LoopbackHub, tag: u32, fog: FogUnit, x: f32) -> Entity {
    hub.server
        .world
        .spawn((
            rep(),
            Tag { id: tag },
            at(x),
            NetProfileRef(FOG_RELEVANCE_PROFILE.into()),
            fog,
        ))
        .id()
}

fn scout(team: u32) -> FogUnit {
    FogUnit::new(team, "scout", SightClass::Unit)
}

fn effect(hub: &mut LoopbackHub, tag: u32, source: Option<Entity>, x: f32) -> Entity {
    hub.server
        .world
        .spawn((
            rep(),
            Tag { id: tag },
            at(x),
            NetProfileRef(FOG_RELEVANCE_PROFILE.into()),
            FogEffect { source },
        ))
        .id()
}

fn move_to(hub: &mut LoopbackHub, e: Entity, x: f32) {
    if let Some(mut t) = hub.server.world.get_mut::<Transform>(e) {
        t.translation.x = x;
    }
}

/// The tags of every entity in a client's world that carries the replicated component.
fn client_tags(hub: &mut LoopbackHub, client: usize) -> BTreeSet<u32> {
    let world = &mut hub.clients[client].world;
    let mut q = world.query::<&Tag>();
    q.iter(world).map(|t| t.id).collect()
}

/// The server's relevance: the connection holds the entity (relevant or lingering).
fn holds(hub: &LoopbackHub, client: usize, e: Entity) -> bool {
    let peer = hub.clients[client].peer;
    hub.server
        .world
        .get::<NetId>(e)
        .is_some_and(|id| hub.server.world.resource::<NetInterest>().holds(peer, *id))
}

/// Default unit sight is 150 m. An enemy walks from 400 m toward a team 1 scout at the origin,
/// 10 m per tick. Until it is inside 150 m, team 1's client never holds it: on no tick does the
/// client world have its component. Once in sight it appears on the next delivered frame.
#[test]
fn a_hidden_enemy_is_never_replicated_to_the_other_team() {
    let mut hub = hub(&[Some(1), Some(2)], strict());
    let _own = unit(&mut hub, 1, scout(1), 0.0);
    let enemy = unit(&mut hub, 2, scout(2), 400.0);
    let mut first_seen: Option<usize> = None;
    let mut appeared: Option<usize> = None;
    for step in 0..30usize {
        let x = 400.0 - 10.0 * step as f32;
        move_to(&mut hub, enemy, x);
        if x <= 150.0 && first_seen.is_none() {
            first_seen = Some(step);
        }
        hub.step();
        let tags = client_tags(&mut hub, 0);
        if tags.contains(&2) {
            assert!(
                first_seen.is_some_and(|s| s < step),
                "step {step}: team 1's client holds the enemy at {x} m before any tick had it in sight"
            );
            appeared.get_or_insert(step);
        }
        if step >= 2 {
            assert!(tags.contains(&1), "step {step}: own scout replicated");
            assert!(
                client_tags(&mut hub, 1).contains(&2),
                "step {step}: the enemy's own team holds it"
            );
        }
    }
    let first_seen = first_seen.expect("the enemy entered sight");
    assert_eq!(first_seen, 25, "400 - 25 x 10 = 150 m: the first tick in sight");
    assert_eq!(
        appeared,
        Some(first_seen + 1),
        "the enemy appears on the frame after the tick it entered sight"
    );
    let status = hub.server.world.resource::<FogRelevance>().status();
    let team1 = status.peers.iter().find(|p| p.team == 1).expect("peer on team 1");
    assert!(
        team1.filtered >= 25,
        "the filter refused the enemy every hidden tick: {team1:?}"
    );
}

/// Allies (shared sight), neutral entities, map features and the dead always reach every team; a
/// live enemy out of sight does not. An observer (team 0) holds everything; a connection with no
/// team holds nothing fogged.
#[test]
fn allies_and_neutral_stay_replicated() {
    let settings = FogRelevanceSettings {
        alliances: vec![(1, 3)],
        ..strict()
    };
    let mut hub = hub(&[Some(1), Some(2), Some(0), None], settings);
    unit(&mut hub, 1, scout(1), 0.0);
    unit(&mut hub, 3, scout(3), 5000.0);
    unit(&mut hub, 10, scout(0), -5000.0);
    unit(
        &mut hub,
        20,
        FogUnit::new(2, "geyser", SightClass::Feature),
        3000.0,
    );
    let mut wreck = scout(2);
    wreck.alive = false;
    unit(&mut hub, 21, wreck, 4000.0);
    unit(&mut hub, 22, scout(2), 6000.0);
    hub.run(4);
    let all: BTreeSet<u32> = [1, 3, 10, 20, 21, 22].into();
    assert_eq!(
        client_tags(&mut hub, 0),
        [1, 3, 10, 20, 21].into(),
        "team 1: own, allied, neutral, feature and dead; not the live enemy"
    );
    assert_eq!(
        client_tags(&mut hub, 1),
        [10, 20, 21, 22].into(),
        "team 2: its own, neutral; team 1 and its ally out of sight"
    );
    assert_eq!(client_tags(&mut hub, 2), all, "an observer holds everything");
    assert!(client_tags(&mut hub, 3).is_empty(), "no team: fail closed");
    let status = hub.server.world.resource::<FogRelevance>().status();
    assert!(status.unmapped_refusals > 0, "{status:?}");
}

/// An enemy that leaves sight stays relevant for exactly `linger_ticks` ticks, then goes; with no
/// linger it goes on the first hidden tick.
#[test]
fn linger_keeps_a_leaving_enemy_for_n_ticks() {
    for linger in [0u32, 5] {
        let settings = FogRelevanceSettings {
            linger_ticks: linger,
            ..strict()
        };
        let mut hub = hub(&[Some(1)], settings);
        unit(&mut hub, 1, scout(1), 0.0);
        let enemy = unit(&mut hub, 2, scout(2), 100.0);
        hub.run(3);
        assert!(holds(&hub, 0, enemy), "in sight: held");
        assert!(client_tags(&mut hub, 0).contains(&2), "in sight: on the client");
        move_to(&mut hub, enemy, 1000.0);
        let mut held = 0u32;
        for step in 1..=(linger + 3) {
            hub.step();
            if holds(&hub, 0, enemy) {
                assert_eq!(held + 1, step, "linger {linger}: held on consecutive ticks only");
                held += 1;
            }
        }
        assert_eq!(held, linger, "linger {linger}: held ticks after leaving sight");
        hub.run(2);
        assert!(
            !client_tags(&mut hub, 0).contains(&2),
            "linger {linger}: gone from the client"
        );
        let status = hub.server.world.resource::<FogRelevance>().status();
        assert_eq!(status.linger_hits_total, u64::from(linger), "{status:?}");
    }
}

/// With `lead_ticks` 4 and a 10 m step the lead ring is 40 m outside sight: an enemy walking in at
/// 10 m per tick is replicated exactly 4 ticks before it enters sight. Without the lead it is
/// replicated on the tick it enters sight.
#[test]
fn lead_margin_replicates_before_sight() {
    for lead in [0u32, 4] {
        let settings = FogRelevanceSettings {
            lead_ticks: lead,
            max_step_m: 10.0,
            ..strict()
        };
        let mut hub = hub(&[Some(1)], settings);
        unit(&mut hub, 1, scout(1), 0.0);
        let enemy = unit(&mut hub, 2, scout(2), 300.0);
        let parked = unit(&mut hub, 3, scout(2), 200.0);
        let (mut first_held, mut first_seen) = (None, None);
        for step in 0..20usize {
            let x = 300.0 - 10.0 * step as f32;
            move_to(&mut hub, enemy, x);
            hub.step();
            if x <= 150.0 {
                first_seen.get_or_insert(step);
            }
            if holds(&hub, 0, enemy) {
                first_held.get_or_insert(step);
            }
            assert!(
                !holds(&hub, 0, parked),
                "lead {lead}: 200 m is outside 150 + 40 m"
            );
        }
        let first_seen = first_seen.expect("entered sight");
        assert_eq!(first_seen, 15);
        assert_eq!(
            first_held,
            Some(first_seen - lead as usize),
            "lead {lead}: replicated lead_ticks before sight"
        );
        let status = hub.server.world.resource::<FogRelevance>().status();
        assert_eq!(status.lead_hits_total, u64::from(lead), "{status:?}");
        assert!((status.lead_margin_m - lead as f32 * 10.0).abs() < 1e-6);
    }
}

/// Eight connections on two teams: each server tick builds one sight vision and one lead vision
/// per team, never one per connection, and the filter answers every connection.
#[test]
fn one_vision_per_team_per_tick() {
    let teams: Vec<Option<u32>> = (0..8).map(|i| Some(1 + i % 2)).collect();
    let mut hub = hub(&teams, FogRelevanceSettings::default());
    for i in 0..6u32 {
        unit(&mut hub, 100 + i, scout(1), i as f32 * 40.0);
        unit(&mut hub, 200 + i, scout(2), 600.0 + i as f32 * 40.0);
    }
    for tick in 1..=10u64 {
        hub.step();
        let status = hub.server.world.resource::<FogRelevance>().status();
        assert_eq!(status.ticks, tick);
        assert_eq!(status.visions_built, 2, "one sight vision per team: {status:?}");
        assert_eq!(
            status.lead_visions_built, 2,
            "one lead vision per team: {status:?}"
        );
        assert_eq!(status.visions_total, 2 * tick);
        assert_eq!(status.teams.len(), 2);
        assert!(status.teams.iter().all(|t| t.peers == 4 && t.watchers == 6));
    }
    let status = hub.server.world.resource::<FogRelevance>().status();
    assert_eq!(status.peers.len(), 8);
    for p in &status.peers {
        assert!(
            p.evaluated > 0 && p.filtered > 0,
            "every connection was filtered: {p:?}"
        );
    }
    for client in 0..8 {
        let own = if client % 2 == 0 { 100 } else { 200 };
        let tags = client_tags(&mut hub, client);
        assert_eq!(tags.len(), 6, "client {client}: its own six only, {tags:?}");
        assert!(tags.iter().all(|t| (own..own + 6).contains(t)));
    }
}

/// A projectile of a hidden shooter is hidden too, unless it flies into sight; `PassAll` shows
/// every effect. `fog_event_shown` answers the same questions for messages.
#[test]
fn effects_of_hidden_entities_follow_the_event_policy() {
    let mut hub = hub(&[Some(1), None], strict());
    let own = unit(&mut hub, 1, scout(1), 0.0);
    let shooter = unit(&mut hub, 2, scout(2), 400.0);
    effect(&mut hub, 30, Some(shooter), 380.0);
    effect(&mut hub, 31, Some(shooter), 100.0);
    effect(&mut hub, 32, Some(own), 1000.0);
    hub.run(3);
    assert_eq!(
        client_tags(&mut hub, 0),
        [1, 31, 32].into(),
        "the shot near the hidden shooter stays hidden; one in sight or from a seen source shows"
    );
    let world = &hub.server.world;
    let (p1, none) = (hub.clients[0].peer, hub.clients[1].peer);
    assert!(!fog_event_shown(world, p1, EventPlace::Entity(shooter.to_bits())));
    assert!(fog_event_shown(world, p1, EventPlace::Entity(own.to_bits())));
    assert!(fog_event_shown(
        world,
        p1,
        EventPlace::At(Vec3::new(100.0, 0.0, 0.0))
    ));
    assert!(!fog_event_shown(
        world,
        p1,
        EventPlace::At(Vec3::new(400.0, 0.0, 0.0))
    ));
    assert!(fog_event_shown(world, p1, EventPlace::Unplaced));
    assert!(
        !fog_event_shown(world, none, EventPlace::Unplaced),
        "no team: fail closed"
    );
    let status = hub.server.world.resource::<FogRelevance>().status();
    assert_eq!((status.events_passed, status.events_dropped), (3, 3));
    let mut settings = strict();
    settings.events = FogEventPolicy::PassAll;
    hub.server
        .world
        .resource_mut::<FogRelevance>()
        .set_settings(settings);
    hub.run(3);
    assert_eq!(
        client_tags(&mut hub, 0),
        [1, 30, 31, 32].into(),
        "pass_all shows every effect"
    );
    let world = &hub.server.world;
    assert!(fog_event_shown(world, p1, EventPlace::Entity(shooter.to_bits())));
}

/// Fog off replicates everything to everyone.
#[test]
fn fog_off_replicates_everything() {
    let mut hub = hub(&[Some(1)], strict());
    unit(&mut hub, 1, scout(1), 0.0);
    unit(&mut hub, 2, scout(2), 5000.0);
    hub.run(3);
    assert_eq!(client_tags(&mut hub, 0), [1].into());
    let rules = FogRules {
        enabled: false,
        ..FogRules::default()
    };
    hub.server.world.resource_mut::<FogRelevance>().set_rules(rules);
    hub.run(3);
    assert_eq!(client_tags(&mut hub, 0), [1, 2].into());
}
