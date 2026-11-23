// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.3-8.4 (N214, ADR-8031): interest management that scales with what changed, not
//! with the worst profile or with every spawn, plus distance rate tiers and the combat-heat
//! priority input. A server world driven at fixed steps, no client worlds; every connection
//! acknowledges every state frame at once. Work is measured with the pipeline's own counters
//! (evaluations, copies visited, tier counts), never with a clock.

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::replication::{ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{NetNode, net_apply, net_receive, net_send, server_add_peer};
use regia_net::testing::SplitMix64;
use regia_net::*;
use regia_scene::transform::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

fn spatial(radius: f32) -> NetProfile {
    NetProfile {
        relevance: RelevanceShape::Spatial {
            radius,
            hysteresis: 0.15,
        },
        ..NetProfile::default()
    }
}

/// A dedicated server with the pipeline, the given profiles (the first is the default), one send
/// per tick and an unlimited budget.
fn server(profiles: &[(&str, NetProfile)], cell_m: f32) -> World {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(LocalPeer(Some(PeerId::SERVER)));
    register_replicated::<Hp>(&mut world, ReplicateOptions::default());
    regia_net::runtime::ensure_resources(&mut world);
    install_net_interest(&mut world);
    for (name, p) in profiles {
        world
            .resource_mut::<NetProfiles>()
            .insert(name, p.clone())
            .unwrap_or_else(|e| panic!("profile {name}: {e}"));
    }
    if let Some((name, _)) = profiles.first() {
        world.resource_mut::<NetProfiles>().set_default(name);
    }
    let mut settings = NetSettings::default();
    settings.replication.client_rate_bytes = 50_000_000;
    settings.replication.spawns_per_send = 100_000;
    settings.replication.send_every_ticks = 1;
    settings.replication.grid_cell_m = cell_m;
    world.insert_resource(settings);
    world
}

fn spawn_at(world: &mut World, p: Vec3, profile: Option<&str>) -> Entity {
    world
        .spawn((
            Replicated {
                profile: profile.map(str::to_string),
                ..Replicated::default()
            },
            Transform {
                translation: p,
                ..Transform::default()
            },
            Hp::default(),
        ))
        .id()
}

fn add_viewer(world: &mut World, peer: PeerId, at: Vec3) -> Entity {
    server_add_peer(world, peer);
    world
        .resource_mut::<ServerReplication>()
        .set_wire(peer, ConnWire::Acked);
    world
        .spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            Transform {
                translation: at,
                ..Transform::default()
            },
        ))
        .id()
}

fn tick(world: &mut World) {
    net_receive(world);
    net_apply(world);
    net_send(world);
    world.resource_mut::<NetNode>().outbox.clear();
    let peers: Vec<PeerId> = world.resource::<NetInterest>().peers.keys().copied().collect();
    let mut rep = world.resource_mut::<ServerReplication>();
    for p in peers {
        rep.ack_all_state(p);
    }
}

fn scatter(world: &mut World, n: usize, half: f32, profile: Option<&str>, seed: u64) -> Vec<Entity> {
    let mut rng = SplitMix64::new(seed);
    let mut r = move |lo: f32, hi: f32| lo + (hi - lo) * rng.next_f64() as f32;
    (0..n)
        .map(|_| {
            let p = Vec3::new(r(-half, half), 0.0, r(-half, half));
            spawn_at(world, p, profile)
        })
        .collect()
}

fn move_by(world: &mut World, e: Entity, f: impl FnOnce(&mut Vec3)) {
    if let Some(mut t) = world.get_mut::<Transform>(e) {
        f(&mut t.translation);
    }
}

fn id_of(world: &World, e: Entity) -> NetId {
    *world
        .get::<NetId>(e)
        .unwrap_or_else(|| panic!("{e:?} has no NetId"))
}

fn assert_parity(world: &World, peers: &[PeerId]) {
    let i = world.resource::<NetInterest>();
    for p in peers {
        // Lingering entities are held but no longer relevant; the brute-force pass lists the relevant.
        let pi = &i.peers[p];
        let held: std::collections::BTreeSet<NetId> =
            pi.held_ids().filter(|id| !pi.is_lingering(*id)).collect();
        assert_eq!(
            held,
            i.brute_force(world, *p),
            "held set of {p:?} differs from a brute-force pass"
        );
    }
}

/// Spec 120 § 8.4: one 1 km profile in use does not turn every viewpoint's query into a scan of
/// everything within 1 km. Each band is queried with its own radius.
#[test]
fn a_long_range_profile_does_not_widen_every_query() {
    let mut world = server(&[("near", spatial(50.0)), ("radar", spatial(1000.0))], 64.0);
    scatter(&mut world, 4000, 1000.0, None, 11);
    let radar = spawn_at(&mut world, Vec3::new(900.0, 0.0, 0.0), Some("radar"));
    let viewer = add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    for _ in 0..3 {
        tick(&mut world);
    }
    // The viewer moves, so every candidate is looked at again.
    move_by(&mut world, viewer, |t| t.x += 1.0);
    tick(&mut world);
    let i = world.resource::<NetInterest>();
    let evaluations = i.counters.evaluations;
    // About 13 entities are within 50 m; the 50 m band's cells hold a few dozen more. Out to the
    // radar's 1 km (plus hysteresis) there are about 3,000.
    assert!(
        evaluations <= 200,
        "{evaluations} evaluations for one viewpoint: the query used the 1 km radius for every band"
    );
    assert!(
        i.holds(PeerId(1), id_of(&world, radar)),
        "the 1 km entity is relevant"
    );
    assert_parity(&world, &[PeerId(1)]);
}

/// Spec 120 § 8.4: a held entity is evaluated again only when it or the viewer moved.
#[test]
fn held_entities_that_did_not_move_keep_their_verdict() {
    let mut world = server(&[("near", spatial(200.0))], 16.0);
    scatter(&mut world, 4000, 1000.0, None, 12);
    add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    for _ in 0..4 {
        tick(&mut world);
    }
    let i = world.resource::<NetInterest>();
    let held = i.peers[&PeerId(1)].held_count() as u64;
    assert!(held > 100, "about 125 entities within 200 m, held {held}");
    let evaluations = i.counters.evaluations;
    assert!(
        evaluations < held / 2,
        "a still tick evaluated {evaluations} of {held} held entities: nothing moved"
    );
    assert_parity(&world, &[PeerId(1)]);
}

/// The held-entity shortcut never hides a change: an entity that moves out of range starts its
/// linger, and a viewer that moves re-evaluates everything it holds.
#[test]
fn a_mover_or_a_moving_viewer_is_evaluated_again() {
    let mut world = server(&[("near", spatial(200.0))], 16.0);
    let es = scatter(&mut world, 2000, 600.0, None, 13);
    let viewer = add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    for _ in 0..3 {
        tick(&mut world);
    }
    let held_one = es
        .iter()
        .copied()
        .find(|e| {
            world
                .resource::<NetInterest>()
                .holds(PeerId(1), id_of(&world, *e))
        })
        .unwrap_or_else(|| panic!("something is held"));
    move_by(&mut world, held_one, |t| *t = Vec3::new(5000.0, 0.0, 0.0));
    tick(&mut world);
    let id = id_of(&world, held_one);
    let i = world.resource::<NetInterest>();
    assert!(
        i.peers[&PeerId(1)].is_lingering(id),
        "the mover left range and lingers"
    );
    assert_parity(&world, &[PeerId(1)]);
    move_by(&mut world, viewer, |t| *t = Vec3::new(300.0, 0.0, 0.0));
    tick(&mut world);
    let i = world.resource::<NetInterest>();
    let held = i.peers[&PeerId(1)].held_count() as u64;
    assert!(
        i.counters.evaluations >= held,
        "a moved viewer evaluates everything it holds"
    );
    assert_parity(&world, &[PeerId(1)]);
}

/// Spec 120 § 8.4: a spawn far from a connection costs that connection nothing; the gather visits
/// the new copy, not every copy of every connection, and relevance does not run a full pass.
#[test]
fn a_spawn_or_despawn_does_not_rescan_every_connection() {
    let mut world = server(&[("near", spatial(200.0))], 32.0);
    let es = scatter(&mut world, 4000, 1000.0, None, 14);
    add_viewer(&mut world, PeerId(1), Vec3::new(-600.0, 0.0, 0.0));
    add_viewer(&mut world, PeerId(2), Vec3::new(600.0, 0.0, 0.0));
    for _ in 0..4 {
        tick(&mut world);
    }
    let held: u64 = world
        .resource::<NetInterest>()
        .peers
        .values()
        .map(|p| p.held_count() as u64)
        .sum();
    assert!(held > 200, "both connections hold their area, {held}");
    let shot = spawn_at(&mut world, Vec3::new(600.0, 0.0, 10.0), None);
    tick(&mut world);
    let visited = world.resource::<ServerReplication>().copies_visited();
    assert!(
        visited <= 16,
        "a spawn made the gather visit {visited} copies (of {held} held): every connection rescanned"
    );
    let evaluations = world.resource::<NetInterest>().counters.evaluations;
    assert!(
        evaluations <= 200,
        "a spawn ran {evaluations} evaluations: a full relevance pass"
    );
    let shot_id = id_of(&world, shot);
    let i = world.resource::<NetInterest>();
    assert!(
        i.holds(PeerId(2), shot_id),
        "the near connection holds the new entity"
    );
    assert!(!i.holds(PeerId(1), shot_id), "the far one does not");
    assert_parity(&world, &[PeerId(1), PeerId(2)]);
    // The spawn reached the near connection's copies on the same tick.
    assert!(
        world
            .resource::<ServerReplication>()
            .conn_holds(PeerId(2), shot_id)
    );
    // A despawn is as cheap, and the copy goes.
    let gone = es
        .iter()
        .copied()
        .find(|e| {
            world
                .resource::<NetInterest>()
                .holds(PeerId(2), id_of(&world, *e))
        })
        .unwrap_or_else(|| panic!("something is held"));
    let gone_id = id_of(&world, gone);
    world.despawn(gone);
    tick(&mut world);
    let visited = world.resource::<ServerReplication>().copies_visited();
    assert!(visited <= 16, "a despawn made the gather visit {visited} copies");
    assert!(
        !world
            .resource::<ServerReplication>()
            .conn_holds(PeerId(2), gone_id)
    );
    assert!(!world.resource::<NetInterest>().holds(PeerId(2), gone_id));
    assert_parity(&world, &[PeerId(1), PeerId(2)]);
}

/// Spec 120 § 8.4: an ownership change visits the copies whose owner flipped, and the new owner
/// holds its entity wherever it is (owner rule).
#[test]
fn an_ownership_change_visits_only_the_flipped_copies() {
    let mut world = server(&[("near", spatial(200.0))], 32.0);
    let es = scatter(&mut world, 4000, 1000.0, None, 15);
    add_viewer(&mut world, PeerId(1), Vec3::new(-600.0, 0.0, 0.0));
    add_viewer(&mut world, PeerId(2), Vec3::new(600.0, 0.0, 0.0));
    for _ in 0..4 {
        tick(&mut world);
    }
    // An entity near connection 1 becomes connection 2's.
    let given = es
        .iter()
        .copied()
        .find(|e| {
            world
                .resource::<NetInterest>()
                .holds(PeerId(1), id_of(&world, *e))
        })
        .unwrap_or_else(|| panic!("something is held"));
    world.entity_mut(given).insert(NetOwner(PeerId(2)));
    tick(&mut world);
    let visited = world.resource::<ServerReplication>().copies_visited();
    assert!(
        visited <= 16,
        "an owner change made the gather visit {visited} copies"
    );
    let id = id_of(&world, given);
    let i = world.resource::<NetInterest>();
    assert!(
        i.holds(PeerId(2), id),
        "the new owner holds its entity across the map"
    );
    assert!(i.holds(PeerId(1), id), "the old viewer still sees it");
    assert!(world.resource::<ServerReplication>().conn_holds(PeerId(2), id));
    assert_parity(&world, &[PeerId(1), PeerId(2)]);
    // And back to the server: the far owner lets go (it lingers, then leaves).
    world.entity_mut(given).remove::<NetOwner>();
    tick(&mut world);
    let i = world.resource::<NetInterest>();
    assert!(
        i.peers[&PeerId(2)].is_lingering(id),
        "the old owner lingers on its far entity"
    );
    assert_parity(&world, &[PeerId(1), PeerId(2)]);
}
