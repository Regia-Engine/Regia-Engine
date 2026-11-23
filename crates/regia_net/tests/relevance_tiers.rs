// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.3 (N214, ADR-8031): distance rate tiers, the combat-heat priority input, the
//! `.netprofile.ron` v2 `rate_tiers` field, and the counters of the held-entity shortcut. A server
//! world driven at fixed steps, no client worlds; every connection acknowledges every state frame
//! at once. Rates are counted in ticks, never with a clock.

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

/// Spec 120 § 8.4: the held-entity shortcut's counter: a still tick keeps every verdict, a moved
/// viewer keeps none.
#[test]
fn the_held_shortcut_is_counted() {
    let mut world = server(&[("near", spatial(200.0))], 16.0);
    scatter(&mut world, 2000, 600.0, None, 21);
    let viewer = add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    for _ in 0..4 {
        tick(&mut world);
    }
    let i = world.resource::<NetInterest>();
    let held = i.peers[&PeerId(1)].held_count() as u64;
    assert!(held > 100, "held {held}");
    assert_eq!(i.counters.held_kept, held, "every held entity kept its verdict");
    move_by(&mut world, viewer, |t| t.x += 0.5);
    tick(&mut world);
    assert_eq!(
        world.resource::<NetInterest>().counters.held_kept,
        0,
        "a moved viewer keeps none"
    );
    let bands: Vec<f32> = world.resource::<NetInterest>().grid.band_radii().collect();
    assert_eq!(bands, vec![200.0], "one band per radius in use");
    assert_parity(&world, &[PeerId(1)]);
}

/// Spec 120 § 8.3 [D27]: distance rate tiers. Over 300 ticks a near entity goes every send, a mid
/// one every third send and a far one at 2 Hz; all move every tick.
#[test]
fn distance_tiers_send_far_entities_at_their_tier_rate() {
    let tiered = NetProfile {
        rate_tiers: Some(RateTiers::default()),
        ..spatial(1000.0)
    };
    let mut world = server(&[("tiered", tiered)], 64.0);
    let near = spawn_at(&mut world, Vec3::new(100.0, 0.0, 0.0), None);
    let mid = spawn_at(&mut world, Vec3::new(0.0, 0.0, 300.0), None);
    let far = spawn_at(&mut world, Vec3::new(-800.0, 0.0, 0.0), None);
    let untiered_far = {
        world
            .resource_mut::<NetProfiles>()
            .insert("plain", spatial(1000.0))
            .unwrap_or_else(|e| panic!("{e}"));
        spawn_at(&mut world, Vec3::new(0.0, 0.0, -800.0), Some("plain"))
    };
    add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    // This test counts the tiers' send rates with exact records. With the far record
    // (ADR-10124, on by default) the far entity below, which climbs 1 cm a tick, has news only
    // when it crosses a coarse step of 1.024 m on y; `tests/far_tier_record.rs` tests that form
    // and its 2 Hz rate on a walking crowd.
    world.resource_mut::<ServerReplication>().set_far_records(false);
    for _ in 0..3 {
        tick(&mut world);
    }
    let hz = regia_net::runtime::tick_hz(&world);
    let ids = [near, mid, far, untiered_far].map(|e| id_of(&world, e));
    let mut sends = [0u32; 4];
    // Each entity's last send before the measured ticks: a change from it is a send.
    let mut last = {
        let pi = &world.resource::<NetInterest>().peers[&PeerId(1)];
        ids.map(|id| pi.send_state(id).and_then(|s| s.last_sent))
    };
    assert!(
        last.iter().all(Option::is_some),
        "everything was sent during the warm-up"
    );
    let mut tier_sent = [0u64; 3];
    let mut tier_held = 0u64;
    const TICKS: u32 = 300;
    for t in 0..TICKS {
        for e in [near, mid, far, untiered_far] {
            move_by(&mut world, e, |p| p.y = 0.01 * (t as f32 + 1.0));
        }
        tick(&mut world);
        let i = world.resource::<NetInterest>();
        let pi = &i.peers[&PeerId(1)];
        for (k, id) in ids.iter().enumerate() {
            let sent = pi.send_state(*id).and_then(|s| s.last_sent);
            if sent != last[k] {
                sends[k] += 1;
                last[k] = sent;
            }
        }
        for (sum, n) in tier_sent.iter_mut().zip(pi.send.counters.tier_sent) {
            *sum += u64::from(n);
        }
        tier_held += u64::from(pi.send.counters.tier_held);
    }
    let far_interval = (hz / 2.0).ceil() as u32;
    assert_eq!(sends[0], TICKS, "near goes every send");
    assert!(
        sends[1].abs_diff(TICKS / 3) <= 1,
        "mid every third send: {}",
        sends[1]
    );
    assert!(
        sends[2].abs_diff(TICKS / far_interval) <= 1,
        "far at 2 Hz ({far_interval} ticks at {hz} Hz): {}",
        sends[2]
    );
    assert_eq!(sends[3], TICKS, "a profile without tiers sends every time");
    assert_eq!(tier_sent[0], u64::from(sends[0] + sends[3]), "near counter");
    assert_eq!(tier_sent[1], u64::from(sends[1]), "mid counter");
    assert_eq!(tier_sent[2], u64::from(sends[2]), "far counter");
    assert!(tier_held > 0, "the held-back updates are counted");
}

/// Spec 120 § 8.3 [D27]: combat heat raises an entity's priority near the viewer and fades with
/// distance; beyond its radius it adds nothing.
#[test]
fn combat_heat_raises_priority_near_the_viewer() {
    let mut world = server(&[("near", spatial(200.0))], 32.0);
    let hot_near = spawn_at(&mut world, Vec3::new(20.0, 0.0, 0.0), None);
    let cold_near = spawn_at(&mut world, Vec3::new(-20.0, 0.0, 0.0), None);
    // Both pairs sit on the X axis: the same distance and the same view cone, so only the heat differs.
    let hot_far = spawn_at(&mut world, Vec3::new(80.0, 0.0, 0.0), None);
    let cold_far = spawn_at(&mut world, Vec3::new(-80.0, 0.0, 0.0), None);
    add_viewer(&mut world, PeerId(1), Vec3::ZERO);
    for _ in 0..3 {
        tick(&mut world);
    }
    // No budget for updates from now on: the accumulators only grow.
    world.resource_mut::<NetSettings>().replication.client_rate_bytes = 1;
    assert!(net_note_combat(&mut world, hot_near, 1.0));
    assert!(net_note_combat(&mut world, hot_far, 1.0));
    assert!(
        !net_note_combat(&mut world, hot_far, f32::NAN),
        "a non-finite amount is refused"
    );
    for t in 0..5 {
        for e in [hot_near, cold_near, hot_far, cold_far] {
            move_by(&mut world, e, |p| p.y = 0.01 * (t as f32 + 1.0));
        }
        tick(&mut world);
    }
    let i = world.resource::<NetInterest>();
    let acc = |e: Entity| {
        i.peers[&PeerId(1)]
            .send_state(id_of(&world, e))
            .map_or(0.0, |s| s.acc)
    };
    let (hn, cn, hf, cf) = (acc(hot_near), acc(cold_near), acc(hot_far), acc(cold_far));
    // At 20 m of a 50 m heat radius the factor is about 1 + 0.6 (less a little decay).
    assert!(
        hn > cn * 1.4,
        "heat near the viewer raises priority: {hn} vs {cn}"
    );
    assert!(
        (hf - cf).abs() <= cf * 1e-4,
        "heat beyond its radius adds nothing: {hf} vs {cf}"
    );
}

/// `.netprofile.ron` v2 (ADR-8031): `rate_tiers` round-trips, a v1 file still loads without
/// tiers, and out-of-range tiers are refused by name.
#[test]
fn rate_tiers_load_validate_and_round_trip() {
    let v1 = NetProfile::from_ron("(format_version: 1, priority: 2.0)").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v1.rate_tiers, None);
    let p = NetProfile {
        rate_tiers: Some(RateTiers {
            near_m: 100.0,
            mid_m: 400.0,
            mid_every: 2,
            far_hz: 5.0,
        }),
        ..NetProfile::default()
    };
    let text = p.to_ron().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(NetProfile::from_ron(&text), Ok(p));
    let defaults =
        NetProfile::from_ron("(format_version: 2, rate_tiers: Some(()))").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(defaults.rate_tiers, Some(RateTiers::default()));
    for (text, field) in [
        ("(rate_tiers: Some((mid_every: 0)))", "rate_tiers.mid_every"),
        ("(rate_tiers: Some((near_m: 700.0)))", "rate_tiers.mid_m"),
        ("(rate_tiers: Some((far_hz: 0.0)))", "rate_tiers.far_hz"),
        ("(rate_tiers: Some((near_m: -1.0)))", "rate_tiers.near_m"),
    ] {
        match NetProfile::from_ron(text) {
            Err(ProfileError::Invalid { field: f, .. }) => assert_eq!(f, field, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    let t = RateTiers::default();
    assert_eq!(t.tier(149.0), RateTier::Near);
    assert_eq!(t.tier(150.0), RateTier::Mid);
    assert_eq!(t.tier(600.0), RateTier::Far);
    assert_eq!(t.interval_ticks(RateTier::Near, 2, 60.0), 2);
    assert_eq!(t.interval_ticks(RateTier::Mid, 2, 60.0), 6);
    assert_eq!(t.interval_ticks(RateTier::Far, 2, 60.0), 30);
    assert_eq!(
        t.interval_ticks(RateTier::Far, 4, 60.0),
        32,
        "whole send intervals, never faster than 2 Hz"
    );
}
