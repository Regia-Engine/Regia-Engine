// SPDX-License-Identifier: MPL-2.0
//! Destruction over the network (ADR-1967): the server decides, clients replay. Runs the sample
//! wall and bridge over `LoopbackHub` at fixed steps, with no sockets and no wall clock, under loss,
//! delay, duplication and reordering, and checks replicas against the server and the pinned hash.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::FixedTime;
use regia_core::math::{EulerRot, Quat};
use regia_ecs::ecs::message::Messages;
use regia_ecs::prelude::*;
use regia_framework::destruction_net::{
    DestructionMessage, DestructionNet, asset_key, destruction_net_client, destruction_net_server,
    install_destruction_net,
};
use regia_net::codec::{CodecContext, ReplicateCodec};
use regia_net::model::NetMode;
use regia_net::testing::{LinkConditions, LoopbackHub, NetProcess};
use regia_physics::destruction::sample::{
    SAMPLE_TICKS, SampleIds, define_sample, sample_assets, sample_hits,
};
use regia_physics::destruction::{
    Authority, BoxGrid, ChunkState, Damage, DamageKind, DestructibleAsset, DestructibleId, DestructiblePose,
    Destruction, DestructionBroke, DestructionFxMessage, DestructionMaterial, GridSupport,
    NavmeshInvalidated, step_destruction,
};

/// `run_sample_headless`'s structure hash (`regia_physics/tests/destruction.rs`): a replica replays
/// its way to this very number.
const PINNED_STRUCTURE_HASH: u64 = 0x4c7d_ecfd_e4c7_d135;

/// Instances in the crowded world: its snapshot is past 1 MiB.
const CROWD: usize = 30_000;

fn setup(world: &mut World) {
    let mode = *world.resource::<NetMode>();
    install_destruction_net(world);
    let authority = if mode.is_client() {
        Authority::Replica
    } else {
        Authority::Server
    };
    world.insert_resource(Destruction::new(authority));
    world.insert_resource(FixedTime::default());
    world.init_resource::<Messages<DestructionBroke>>();
    world.init_resource::<Messages<DestructionFxMessage>>();
    world.init_resource::<Messages<NavmeshInvalidated>>();
    let (wall, bridge) = sample_assets().expect("sample assets");
    let mut net = world.resource_mut::<DestructionNet>();
    net.register_asset(wall);
    net.register_asset(bridge);
}

fn schedule(p: &mut NetProcess) {
    p.gameplay
        .add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
}

fn hub(mode: NetMode, seed: u64, clients: usize) -> LoopbackHub {
    let mut hub = LoopbackHub::new(mode, seed, setup);
    schedule(&mut hub.server);
    for _ in 0..clients {
        join(&mut hub);
    }
    hub
}

fn join(hub: &mut LoopbackHub) -> usize {
    let i = hub.add_client();
    schedule(&mut hub.clients[i]);
    i
}

fn world_of(p: &NetProcess) -> &regia_physics::destruction::DestructionWorld {
    &p.world.resource::<Destruction>().world
}

fn stats_of(p: &NetProcess) -> regia_framework::destruction_net::DestructionNetStats {
    p.world.resource::<DestructionNet>().stats().clone()
}

fn hit_next_tick(hub: &mut LoopbackHub, ids: SampleIds) {
    let tick = world_of(&hub.server).tick() + 1;
    let mut d = hub.server.world.resource_mut::<Destruction>();
    for hit in sample_hits(tick, ids) {
        d.world.damage(hit).expect("hit");
    }
}

fn define(hub: &mut LoopbackHub) -> SampleIds {
    define_sample(&mut hub.server.world.resource_mut::<Destruction>().world).expect("define")
}

/// Runs the whole script, then `drain` more steps.
fn run_script(hub: &mut LoopbackHub, ids: SampleIds, drain: usize) {
    for _ in 0..SAMPLE_TICKS {
        hit_next_tick(hub, ids);
        hub.step();
    }
    hub.run(drain);
}

#[test]
fn clients_replay_their_way_to_the_pinned_hash() {
    let mut hub = hub(NetMode::DedicatedServer, 1, 3);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 8);
    assert_eq!(world_of(&hub.server).structure_hash(), PINNED_STRUCTURE_HASH);
    let server = stats_of(&hub.server);
    assert!(server.compact_breaks_sent > 0);
    assert_eq!(
        server.full_breaks_sent, 0,
        "nothing moved: every break fits the short form"
    );
    assert_eq!(server.send_errors, 0);
    assert_eq!(server.peers, 3);
    for c in &hub.clients {
        assert_eq!(world_of(c).structure_hash(), PINNED_STRUCTURE_HASH);
        assert_eq!(world_of(c).event_count(), world_of(&hub.server).event_count());
        let s = stats_of(c);
        assert_eq!(s.snapshots_applied, 1);
        assert!(s.checks_passed > 4, "{s:?}");
        assert_eq!((s.checks_failed, s.resyncs_requested), (0, 0));
        assert_eq!(s.last_problem, None);
        assert!(c.world.resource::<DestructionNet>().in_step(NetMode::Client));
    }
}

#[test]
fn a_bad_link_changes_when_breaks_arrive_never_what_they_are() {
    for seed in 0..8 {
        let mut hub = hub(NetMode::DedicatedServer, seed, 2);
        hub.down = LinkConditions {
            loss: 0.25,
            delay_ticks: 3 + seed % 4,
            duplicate: 0.1,
            reorder: 0.2,
            resend_ticks: 3,
            reliable_extra_ticks: seed % 3,
        };
        hub.up = hub.down;
        let ids = define(&mut hub);
        run_script(&mut hub, ids, 200);
        for c in &hub.clients {
            assert_eq!(world_of(c).structure_hash(), PINNED_STRUCTURE_HASH, "seed {seed}");
            assert_eq!(stats_of(c).checks_failed, 0, "seed {seed}");
        }
    }
}

#[test]
fn every_state_a_replica_passes_through_is_one_the_server_had() {
    let mut hub = hub(NetMode::DedicatedServer, 7, 1);
    hub.down.delay_ticks = 5;
    let ids = define(&mut hub);
    let mut history: BTreeMap<u64, u64> = BTreeMap::new();
    let mut seen = 0;
    for step in 0..SAMPLE_TICKS + 20 {
        if step < SAMPLE_TICKS {
            hit_next_tick(&mut hub, ids);
        }
        hub.step();
        let w = world_of(&hub.server);
        history.insert(w.event_count(), w.structure_hash());
        let c = world_of(&hub.clients[0]);
        // Counts above 0 name whole ticks of the server; 0 is also a client without its snapshot.
        if let Some(hash) = history.get(&c.event_count()).filter(|_| c.event_count() > 0) {
            assert_eq!(c.structure_hash(), *hash, "at {} events", c.event_count());
            seen += 1;
        }
    }
    assert!(seen > 40, "{seen}");
}

#[test]
fn a_late_joiner_catches_up_from_a_snapshot_and_follows_the_stream() {
    let mut hub = hub(NetMode::DedicatedServer, 3, 1);
    let ids = define(&mut hub);
    for _ in 0..55 {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    let before = world_of(&hub.server).event_count();
    assert!(before > 0, "the first hits landed");
    let late = join(&mut hub);
    for _ in 55..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    hub.run(8);
    assert!(
        world_of(&hub.server).event_count() > before,
        "more broke after the join"
    );
    let c = &hub.clients[late];
    assert_eq!(world_of(c).structure_hash(), PINNED_STRUCTURE_HASH);
    let s = stats_of(c);
    assert_eq!(s.snapshots_applied, 1);
    assert!(
        s.breaks_applied < world_of(&hub.server).event_count(),
        "the old ones came in the snapshot"
    );
    assert!(s.breaks_applied > 0);
    for (id, inst) in world_of(&hub.server).instances() {
        let r = world_of(c).instance(id).expect("instance");
        assert_eq!(r.chunk_states(), inst.chunk_states());
        assert_eq!(r.bond_broken(), inst.bond_broken());
        assert_eq!(r.seed(), inst.seed());
    }
}

/// A two-chunk block: cheap to walk, so a crowd of them is all snapshot bytes.
fn pebble() -> Arc<DestructibleAsset> {
    Arc::new(
        DestructibleAsset::box_grid(&BoxGrid {
            name: "pebble".into(),
            counts: [2, 1, 1],
            cell: [0.5, 0.5, 0.5],
            density: 2000.0,
            strength_per_area: 200.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid"),
    )
}

/// The sample scene plus a crowd whose snapshot is past the 1 MiB one tick may queue for a peer.
fn crowded_hub(parts_per_tick: Option<u32>) -> (LoopbackHub, SampleIds) {
    let mut hub = hub(NetMode::DedicatedServer, 11, 0);
    if let Some(parts) = parts_per_tick {
        hub.server
            .world
            .resource_mut::<DestructionNet>()
            .config
            .snapshot_parts_per_tick = parts;
    }
    let ids = define(&mut hub);
    let asset = pebble();
    {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        for i in 0..CROWD {
            let pose = DestructiblePose {
                translation: [(i % 200) as f32, 40.0, (i / 200) as f32],
                rotation: Quat::IDENTITY.to_array(),
            };
            d.world.define(asset.clone(), pose, i as u64).expect("define");
        }
    }
    hub.run(2);
    (hub, ids)
}

fn join_crowd(hub: &mut LoopbackHub) -> usize {
    let late = join(hub);
    hub.clients[late]
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(pebble());
    late
}

#[test]
fn a_late_joiner_gets_a_world_past_one_ticks_queue_while_it_keeps_breaking() {
    let (mut hub, ids) = crowded_hub(None);
    let late = join_crowd(&mut hub);
    let mut ticks_sending = 0;
    for _ in 0..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        hub.step();
        ticks_sending += u32::from(stats_of(&hub.server).snapshots_in_progress > 0);
    }
    assert!(ticks_sending > 20, "1 MiB takes over 20 parts: {ticks_sending}");
    hub.run(8);
    let server = stats_of(&hub.server);
    assert_eq!(server.peers, 1, "the joiner was not dropped");
    assert_eq!(server.snapshots_in_progress, 0, "the send finished");
    assert_eq!((server.snapshots_sent, server.send_errors), (1, 0));
    let c = &hub.clients[late];
    let s = stats_of(c);
    assert_eq!(s.snapshots_applied, 1);
    assert_eq!(s.resyncs_requested, 0, "it never lost step");
    assert_eq!(s.checks_failed, 0);
    assert!(
        s.breaks_applied > 0,
        "breaks during the send came through the stream"
    );
    assert_eq!(world_of(c).instances().count(), CROWD + 2);
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
    assert!(c.world.resource::<DestructionNet>().in_step(NetMode::Client));
}

#[test]
fn no_parts_setting_can_overflow_a_ticks_queue() {
    let (mut hub, _) = crowded_hub(Some(u32::MAX));
    let late = join_crowd(&mut hub);
    hub.step();
    assert!(
        stats_of(&hub.server).snapshots_in_progress > 0,
        "the setting is capped, so the send takes more than one tick"
    );
    hub.run(12);
    assert_eq!(stats_of(&hub.server).peers, 1, "the joiner was not dropped");
    assert_eq!(stats_of(&hub.clients[late]).snapshots_applied, 1);
}

#[test]
fn a_replica_that_loses_step_during_a_long_snapshot_gets_a_new_one() {
    let (mut hub, ids) = crowded_hub(None);
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .config
        .resync_cooldown_ticks = 2;
    let late = join_crowd(&mut hub);
    // Short enough that a restart per ask would never finish, if parts did not count as answers.
    hub.clients[late]
        .world
        .resource_mut::<DestructionNet>()
        .config
        .resync_retry_ticks = 6;
    hub.run(4);
    assert!(stats_of(&hub.server).snapshots_in_progress > 0, "still sending");
    assert!(
        !hub.clients[late]
            .world
            .resource::<DestructionNet>()
            .in_step(NetMode::Client)
    );
    hub.clients[late]
        .world
        .resource_mut::<DestructionNet>()
        .request_resync();
    for _ in 0..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    hub.run(60);
    let c = &hub.clients[late];
    let s = stats_of(c);
    assert_eq!(s.snapshots_applied, 1, "the second send replaced the first");
    assert_eq!(s.resyncs_requested, 1);
    assert_eq!(stats_of(&hub.server).snapshots_sent, 2);
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
}

#[test]
fn a_big_snapshot_arrives_whole_over_a_bad_link() {
    let (mut hub, ids) = crowded_hub(None);
    let bad = LinkConditions {
        loss: 0.1,
        delay_ticks: 2,
        duplicate: 0.05,
        reorder: 0.1,
        ..LinkConditions::default()
    };
    hub.down = bad;
    hub.up = bad;
    let late = join_crowd(&mut hub);
    for _ in 0..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    hub.run(120);
    let c = &hub.clients[late];
    assert_eq!(stats_of(c).snapshots_applied, 1);
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
}

#[test]
fn replicas_get_the_same_effects_in_the_same_order() {
    let mut hub = hub(NetMode::DedicatedServer, 5, 1);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 8);
    let fx = |p: &NetProcess| {
        let m = p.world.resource::<Messages<DestructionFxMessage>>();
        let mut cursor = m.get_cursor();
        cursor.read(m).map(|f| f.0.clone()).collect::<Vec<_>>()
    };
    let broke = |p: &NetProcess| {
        let m = p.world.resource::<Messages<DestructionBroke>>();
        let mut cursor = m.get_cursor();
        cursor.read(m).map(|f| f.0.clone()).collect::<Vec<_>>()
    };
    let (a, b) = (fx(&hub.server), fx(&hub.clients[0]));
    assert!(!a.is_empty());
    assert_eq!(a, b, "same effects, positions, tags and seeds");
    assert_eq!(
        broke(&hub.server),
        broke(&hub.clients[0]),
        "same events, bit for bit"
    );
}

#[test]
fn a_replica_that_lost_step_notices_and_heals() {
    let mut hub = hub(NetMode::DedicatedServer, 9, 2);
    let ids = define(&mut hub);
    for _ in 0..35 {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    // Corrupt client 1: an instance the server never placed changes its hash.
    let (wall, _) = sample_assets().expect("assets");
    hub.clients[1]
        .world
        .resource_mut::<Destruction>()
        .world
        .define_at(DestructibleId(50), wall, DestructiblePose::default(), 1)
        .expect("define");
    let bad = world_of(&hub.clients[1]).structure_hash();
    assert_ne!(bad, world_of(&hub.server).structure_hash());
    for _ in 35..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    hub.run(140);
    let s = stats_of(&hub.clients[1]);
    assert_eq!(world_of(&hub.clients[1]).structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(s.checks_failed, 1);
    assert_eq!(s.resyncs_requested, 1);
    assert_eq!(s.snapshots_applied, 2);
    assert_eq!(s.last_problem.as_deref(), Some("destruction_hash_mismatch"));
    assert_eq!(stats_of(&hub.server).resyncs_served, 1);
    assert_eq!(
        stats_of(&hub.clients[0]).snapshots_applied,
        1,
        "the other client never noticed"
    );
}

fn big_wall() -> Arc<DestructibleAsset> {
    Arc::new(
        DestructibleAsset::box_grid(&BoxGrid {
            name: "big".into(),
            counts: [20, 20, 1],
            cell: [0.5, 0.5, 0.25],
            density: 2000.0,
            strength_per_area: 200.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid"),
    )
}

#[test]
fn a_burst_is_paced_and_a_trailing_replica_still_arrives() {
    let mut hub = hub(NetMode::DedicatedServer, 2, 1);
    let asset = big_wall();
    hub.clients[0]
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(asset.clone());
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .config
        .max_ops_per_tick = 16;
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(asset, DestructiblePose::default(), 3)
        .expect("define");
    hub.run(2);
    {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        d.world
            .damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.0, 2.0, 0.0],
                    radius: 8.0,
                    impulse: 20_000.0,
                },
            })
            .expect("blast");
    }
    hub.step();
    hub.step();
    let total = world_of(&hub.server).event_count();
    assert!(total > 200, "the blast broke much: {total}");
    let queued = stats_of(&hub.server).queued_ops;
    assert!(queued > 0, "the rest waits for its turn");
    assert!(world_of(&hub.clients[0]).event_count() < total);
    let mut steps = 0;
    while world_of(&hub.clients[0]).event_count() < total && steps < 1000 {
        hub.step();
        steps += 1;
    }
    assert!(steps >= 8, "paced over several ticks: {steps}");
    hub.run(3);
    assert_eq!(
        world_of(&hub.clients[0]).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
    assert_eq!(stats_of(&hub.server).queued_ops, 0);
}

#[test]
fn a_moving_structure_travels_in_full_and_its_pose_follows() {
    let mut hub = hub(NetMode::DedicatedServer, 4, 1);
    let ids = define(&mut hub);
    hub.run(2);
    for step in 0..SAMPLE_TICKS {
        hit_next_tick(&mut hub, ids);
        {
            let mut d = hub.server.world.resource_mut::<Destruction>();
            let pose = DestructiblePose::at([0.0, 0.0, -5.0 - step as f32 * 0.01]);
            d.world.set_pose(ids.wall, pose).expect("pose");
        }
        hub.step();
    }
    hub.run(40);
    let server = stats_of(&hub.server);
    assert!(server.full_breaks_sent > 0, "{server:?}");
    assert!(
        server.compact_breaks_sent > 0,
        "the bridge never moved: {server:?}"
    );
    let c = &hub.clients[0];
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
    assert_eq!(stats_of(c).checks_failed, 0);
    assert_eq!(
        world_of(c).instance(ids.wall).expect("wall").pose(),
        world_of(&hub.server).instance(ids.wall).expect("wall").pose()
    );
}

#[test]
fn a_structure_comes_to_rest_where_the_server_left_it_even_over_a_lossy_link() {
    let mut hub = hub(NetMode::DedicatedServer, 11, 1);
    hub.down.loss = 0.5;
    let ids = define(&mut hub);
    hub.run(4);
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .set_pose(ids.bridge, DestructiblePose::at([1.0, 2.0, 3.0]))
        .expect("pose");
    hub.run(300);
    assert_eq!(
        world_of(&hub.clients[0])
            .instance(ids.bridge)
            .expect("bridge")
            .pose()
            .translation,
        [1.0, 2.0, 3.0]
    );
}

#[test]
fn instances_come_and_go_and_ids_stay_the_servers() {
    let mut hub = hub(NetMode::DedicatedServer, 6, 1);
    let ids = define(&mut hub);
    hub.run(3);
    let removed = {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        let (wall, _) = sample_assets().expect("assets");
        let id = d
            .world
            .define(wall, DestructiblePose::at([9.0, 0.0, 0.0]), 77)
            .expect("define");
        assert!(d.world.remove(ids.bridge));
        id
    };
    hub.run(4);
    let late = join(&mut hub);
    hub.run(4);
    for c in &hub.clients {
        let w = world_of(c);
        assert_eq!(w.structure_hash(), world_of(&hub.server).structure_hash());
        assert!(w.instance(ids.bridge).is_none());
        assert_eq!(w.instance(removed).expect("new one").seed(), 77);
    }
    assert_eq!(stats_of(&hub.clients[late]).snapshots_applied, 1);
}

#[test]
fn an_unknown_asset_holds_the_stream_until_it_is_registered() {
    let mut hub = hub(NetMode::DedicatedServer, 8, 1);
    let asset = big_wall();
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(asset.clone(), DestructiblePose::default(), 1)
        .expect("define");
    hub.run(4);
    assert_eq!(stats_of(&hub.clients[0]).missing_assets, vec![asset_key(&asset)]);
    {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        d.world
            .damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.0, 2.0, 0.0],
                    radius: 3.0,
                    impulse: 20_000.0,
                },
            })
            .expect("blast");
    }
    hub.run(4);
    assert_eq!(
        world_of(&hub.clients[0]).event_count(),
        0,
        "nothing skips the missing asset"
    );
    assert!(stats_of(&hub.clients[0]).pending_items > 0);
    hub.clients[0]
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(asset);
    hub.run(3);
    assert_eq!(
        world_of(&hub.clients[0]).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
    assert!(stats_of(&hub.clients[0]).missing_assets.is_empty());
}

#[test]
fn a_replica_never_decides_and_a_server_never_listens() {
    let mut hub = hub(NetMode::ListenServer, 10, 1);
    let ids = define(&mut hub);
    hub.run(2);
    let refused = hub.clients[0]
        .world
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(ids.wall),
            kind: DamageKind::Impulse {
                chunk: 0,
                impulse: [1.0, 0.0, 0.0],
            },
        })
        .map_err(|e| e.code());
    assert_eq!(refused, Err("destruction_replica"));
    run_script(&mut hub, ids, 8);
    assert_eq!(world_of(&hub.server).authority(), Authority::Server);
    assert_eq!(world_of(&hub.clients[0]).authority(), Authority::Replica);
    assert_eq!(world_of(&hub.clients[0]).structure_hash(), PINNED_STRUCTURE_HASH);
}

#[test]
fn a_client_that_starts_as_a_server_turns_replica_and_loses_its_offline_world() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 12, setup);
    schedule(&mut hub.server);
    let ids = define(&mut hub);
    let i = join(&mut hub);
    // What a game gets when it joins with the world of its menu scene still loaded.
    let (wall, _) = sample_assets().expect("assets");
    {
        let mut d = hub.clients[i].world.resource_mut::<Destruction>();
        d.world.set_authority(Authority::Server);
        d.world
            .define(wall, DestructiblePose::default(), 1)
            .expect("define");
    }
    run_script(&mut hub, ids, 8);
    assert_eq!(world_of(&hub.clients[i]).authority(), Authority::Replica);
    assert_eq!(world_of(&hub.clients[i]).structure_hash(), PINNED_STRUCTURE_HASH);
}

#[test]
fn a_rotated_structure_replays_bit_for_bit() {
    // Quaternions whose second normalization would move a bit: a replica must keep the server's.
    let mut hub = hub(NetMode::DedicatedServer, 16, 1);
    let (wall, bridge) = sample_assets().expect("assets");
    let mut ids = Vec::new();
    {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        for (i, (x, y, z)) in [(0.3f32, 0.2f32, 0.7f32), (1.1, 0.4, 0.9), (0.05, 0.9, 0.2)]
            .into_iter()
            .enumerate()
        {
            let q = Quat::from_euler(EulerRot::XYZ, x, y, z);
            let pose = DestructiblePose {
                translation: [i as f32 * 7.0, 1.0, -3.0],
                rotation: q.to_array(),
            };
            let asset = if i == 1 { bridge.clone() } else { wall.clone() };
            ids.push(d.world.define(asset, pose, 40 + i as u64).expect("define"));
        }
    }
    hub.run(3);
    let middle = |w: &regia_physics::destruction::DestructionWorld, id| {
        let inst = w.instance(id).expect("instance");
        inst.world_centroid(inst.chunk_states().len() as u32 / 2)
            .expect("centre")
    };
    for (step, id) in ids.iter().enumerate() {
        let center = middle(world_of(&hub.server), *id);
        hub.server
            .world
            .resource_mut::<Destruction>()
            .world
            .damage(Damage {
                target: Some(*id),
                kind: DamageKind::Explosion {
                    center,
                    radius: 2.0,
                    impulse: 3000.0,
                },
            })
            .expect("blast");
        hub.run(2 + step);
    }
    hub.run(4);
    let late = join(&mut hub);
    hub.run(4);
    let server = stats_of(&hub.server);
    assert!(world_of(&hub.server).event_count() > 20);
    assert_eq!(server.full_breaks_sent, 0, "nothing moved: {server:?}");
    for c in &hub.clients {
        assert_eq!(
            world_of(c).structure_hash(),
            world_of(&hub.server).structure_hash()
        );
        assert_eq!(stats_of(c).checks_failed, 0);
        for id in &ids {
            assert_eq!(
                world_of(c).instance(*id).expect("instance").pose(),
                world_of(&hub.server).instance(*id).expect("instance").pose(),
                "bit for bit"
            );
        }
    }
    assert_eq!(stats_of(&hub.clients[late]).snapshots_applied, 1);
}

#[test]
fn equal_assets_in_many_arcs_keep_one_key_and_a_freed_address_never_lies() {
    let mut net = DestructionNet::default();
    let first = big_wall();
    let key = net.register_asset(first.clone());
    // Each pass drops an equal `Arc`; the allocator may hand its address to the next one.
    for _ in 0..16 {
        assert_eq!(net.register_asset(big_wall()), key);
    }
    let other = Arc::new(DestructibleAsset {
        name: "other".into(),
        ..DestructibleAsset::clone(&first)
    });
    let other_key = net.register_asset(other.clone());
    assert_ne!(other_key, key);
    assert_eq!(other_key, asset_key(&other));
    assert_eq!(net.register_asset(first), key, "a cached address");
}

#[test]
fn a_held_stream_is_not_in_step_until_it_has_applied() {
    let mut hub = hub(NetMode::DedicatedServer, 17, 1);
    let asset = big_wall();
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(asset.clone(), DestructiblePose::default(), 1)
        .expect("define");
    hub.run(4);
    let in_step = |hub: &LoopbackHub| {
        hub.clients[0]
            .world
            .resource::<DestructionNet>()
            .in_step(NetMode::Client)
    };
    assert!(!in_step(&hub), "the Define is waiting for its asset");
    hub.clients[0]
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(asset);
    hub.run(2);
    assert!(in_step(&hub));
}

#[test]
fn a_session_ending_gives_the_world_back_to_the_process() {
    let mut hub = hub(NetMode::DedicatedServer, 18, 1);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 4);
    // The client leaves the session and carries on alone: its world decides again, as it was.
    let c = &mut hub.clients[0];
    c.world.insert_resource(NetMode::Offline);
    c.tick();
    assert_eq!(world_of(c).authority(), Authority::Server);
    assert_eq!(world_of(c).structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(c.world.resource::<DestructionNet>().client_phase(), "waiting");
}

#[test]
fn offline_nothing_goes_anywhere() {
    let mut hub = hub(NetMode::Offline, 13, 0);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 2);
    assert_eq!(world_of(&hub.server).structure_hash(), PINNED_STRUCTURE_HASH);
    let s = stats_of(&hub.server);
    assert_eq!((s.messages_sent, s.snapshots_sent, s.queued_ops), (0, 0, 0));
}

#[test]
fn a_session_started_after_offline_play_begins_with_a_snapshot_of_it() {
    let mut hub = hub(NetMode::Offline, 19, 0);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 2);
    // Hosting starts: the world played offline is what joiners get.
    hub.server.world.insert_resource(NetMode::ListenServer);
    let c = join(&mut hub);
    hub.run(4);
    assert_eq!(world_of(&hub.clients[c]).structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(stats_of(&hub.clients[c]).snapshots_applied, 1);
    assert_eq!(stats_of(&hub.clients[c]).checks_failed, 0);
}

#[test]
fn a_burst_costs_a_few_bytes_per_break() {
    // What the wire actually carries for the sample script, in the short and the long form.
    let mut hub = hub(NetMode::DedicatedServer, 14, 1);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 2);
    let world = world_of(&hub.server);
    let events: Vec<_> = world.recent_events().cloned().collect();
    assert!(!events.is_empty());
    let cx = CodecContext::detached();
    let size = |ops: Vec<regia_framework::destruction_net::WireOp>| {
        let m = DestructionMessage::Stream(regia_framework::destruction_net::StreamBatch {
            first_seq: 0,
            base_tick: 0,
            ops,
        });
        let mut w = regia_net::bits::BitWriter::new();
        m.encode(&mut w, &cx);
        w.finish().len()
    };
    let compact: Vec<_> = events
        .iter()
        .map(|e| regia_framework::destruction_net::WireOp::Compact {
            id: e.destructible,
            tick: e.tick,
            kind: world.compact_event(e).expect("the sample's breaks all compact"),
        })
        .collect();
    let full: Vec<_> = events
        .iter()
        .map(|e| regia_framework::destruction_net::WireOp::Full {
            id: e.destructible,
            tick: e.tick,
            kind: e.kind.clone(),
            seed: e.seed,
        })
        .collect();
    let (short, long) = (size(compact), size(full));
    println!(
        "DESTRUCTION_NET_BYTES events={} short={short} long={long} per_event={:.1}",
        events.len(),
        short as f64 / events.len() as f64
    );
    assert!(short * 2 < long, "{short} against {long}");
    assert!(
        short / events.len() <= 16,
        "{short} bytes for {} events",
        events.len()
    );
}

#[test]
fn replica_states_hold_for_every_chunk_after_a_snapshot() {
    let mut hub = hub(NetMode::DedicatedServer, 15, 0);
    let ids = define(&mut hub);
    run_script(&mut hub, ids, 2);
    let late = join(&mut hub);
    hub.run(3);
    let w = world_of(&hub.clients[late]);
    let inst = w.instance(ids.bridge).expect("bridge");
    assert!(inst.chunk_states().contains(&ChunkState::Detached));
    assert_eq!(w.structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(stats_of(&hub.clients[late]).breaks_applied, 0);
}

// ADR-2305: long snapshots under a full stream, instances past 64 KiB, relevance.

/// Pebbles defined and removed each tick: 128 of each is 256 ops, the default `max_ops_per_tick`.
fn churn(hub: &mut LoopbackHub, live: &mut Vec<DestructibleId>, asset: &Arc<DestructibleAsset>, tick: usize) {
    let mut d = hub.server.world.resource_mut::<Destruction>();
    for id in live.drain(..) {
        d.world.remove(id);
    }
    for k in 0..128 {
        let pose = DestructiblePose {
            translation: [k as f32, 80.0, tick as f32],
            rotation: Quat::IDENTITY.to_array(),
        };
        live.push(
            d.world
                .define(asset.clone(), pose, (tick * 128 + k) as u64)
                .expect("define"),
        );
    }
}

#[test]
fn a_snapshot_that_takes_over_300_ticks_under_a_full_stream_keeps_step() {
    let (mut hub, ids) = crowded_hub(None);
    // 2 KiB parts: the crowd's 1 MiB takes about 500 ticks.
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .config
        .snapshot_part_bytes = 2048;
    let late = join_crowd(&mut hub);
    let asset = pebble();
    let mut live = Vec::new();
    let mut sending = 0u32;
    for tick in 0..420 {
        if (tick as u64) < SAMPLE_TICKS {
            hit_next_tick(&mut hub, ids);
        }
        churn(&mut hub, &mut live, &asset, tick);
        hub.step();
        sending += u32::from(stats_of(&hub.server).snapshots_in_progress > 0);
        let c = stats_of(&hub.clients[late]);
        assert_eq!(c.last_problem, None, "tick {tick}: {c:?}");
    }
    assert!(sending >= 300, "the snapshot took {sending} ticks");
    // The churn stops; the joiner gets its snapshot and catches up on what queued meanwhile.
    let mut steps = 0;
    loop {
        hub.step();
        steps += 1;
        let server = stats_of(&hub.server);
        let c = &hub.clients[late];
        if server.snapshots_in_progress == 0
            && server.catching_up == 0
            && c.world.resource::<DestructionNet>().in_step(NetMode::Client)
            && world_of(c).event_count() == world_of(&hub.server).event_count()
        {
            break;
        }
        assert!(steps < 2000, "never caught up: {server:?}");
    }
    hub.run(4);
    let server = stats_of(&hub.server);
    assert_eq!(
        (server.snapshots_sent, server.send_errors, server.backlog_resyncs),
        (1, 0, 0)
    );
    let c = &hub.clients[late];
    let s = stats_of(c);
    assert_eq!(s.snapshots_applied, 1);
    assert_eq!((s.resyncs_requested, s.checks_failed), (0, 0), "{s:?}");
    assert_eq!(s.last_problem, None);
    assert!(s.checks_passed > 0);
    assert_eq!(
        world_of(c).instances().count(),
        world_of(&hub.server).instances().count()
    );
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
}

/// An asset with every chunk bonded to every other: enough bonds that one instance's broken list
/// passes 256 KiB.
fn dense_asset(chunks: u32) -> Arc<DestructibleAsset> {
    let mut asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "dense".into(),
        counts: [chunks, 1, 1],
        cell: [0.25, 0.25, 0.25],
        density: 2000.0,
        strength_per_area: 1.0e9,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    let template = asset.bonds[0].clone();
    asset.bonds.clear();
    for a in 0..chunks {
        for b in a + 1..chunks {
            asset.bonds.push(regia_physics::destruction::DestructibleBond {
                a,
                b,
                ..template.clone()
            });
        }
    }
    asset.validate().expect("valid dense asset");
    Arc::new(asset)
}

/// The largest instance the destructible caps allow reaches a late joiner in pieces (ADR-5960 (3)).
/// Under `MAX_DESTRUCTIBLE_BONDS` no instance's broken list can pass 256 KiB, so the older test of
/// such an instance could not build its asset and was removed (ADR-5993: no spec sets that size);
/// this one, every bond broken but a chain, still passes one snapshot part, so it is split, sent and
/// joined back whole.
#[test]
fn the_largest_instance_the_bond_cap_allows_reaches_a_joiner_in_pieces() {
    // Every chunk bonded to every other: the most chunks whose complete graph fits under the cap.
    let cap = regia_physics::destruction::MAX_DESTRUCTIBLE_BONDS;
    let chunks = (1..)
        .take_while(|n: &usize| n * (n - 1) / 2 <= cap)
        .last()
        .unwrap_or(2);
    let asset = dense_asset(chunks as u32);
    let bonds = asset.bonds.len() as u32;
    assert!(
        asset.bonds.len() <= cap && (chunks + 1) * chunks / 2 > cap,
        "{chunks} chunks, {bonds} bonds"
    );
    let mut hub = hub(NetMode::DedicatedServer, 31, 0);
    let id = {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        let id = d
            .world
            .define(asset.clone(), DestructiblePose::at([0.0, 0.0, 0.0]), 7)
            .expect("define");
        let mut snapshot = d.world.snapshot();
        let chain: std::collections::BTreeSet<u32> = asset
            .bonds
            .iter()
            .enumerate()
            .filter(|(_, b)| b.b == b.a + 1)
            .map(|(i, _)| i as u32)
            .collect();
        snapshot.instances[0].broken = (0..bonds).filter(|b| !chain.contains(b)).collect();
        // One byte a broken bond: the list alone passes one snapshot part.
        assert!(
            snapshot.instances[0].broken.len()
                > regia_framework::destruction_net::MAX_SNAPSHOT_PART_BYTES as usize
        );
        d.world.set_authority(Authority::Replica);
        let lookup = |_: DestructibleId| Some(asset.clone());
        d.world.restore(&snapshot, &lookup).expect("restore");
        d.world.set_authority(Authority::Server);
        id
    };
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(asset.clone());
    hub.run(2);
    let late = join(&mut hub);
    hub.clients[late]
        .world
        .resource_mut::<DestructionNet>()
        .register_asset(asset.clone());
    hub.run(12);
    let server = stats_of(&hub.server);
    assert_eq!(server.send_errors, 0, "no part was refused: {server:?}");
    assert_eq!(server.split_instances, 1);
    assert_eq!(server.snapshots_sent, 1);
    let c = &hub.clients[late];
    let s = stats_of(c);
    assert_eq!(s.snapshots_applied, 1, "{s:?}");
    assert_eq!((s.resyncs_requested, s.checks_failed), (0, 0));
    assert_eq!(
        world_of(c).instance_snapshot(id),
        world_of(&hub.server).instance_snapshot(id)
    );
    assert_eq!(
        world_of(c).structure_hash(),
        world_of(&hub.server).structure_hash()
    );
}

/// A structure bigger than the relevance-gain budget is not counted as gained until its last
/// piece is queued, and the client then matches the server.
#[test]
fn a_relevance_gain_larger_than_the_byte_budget_arrives_over_several_ticks() {
    let asset = dense_asset(50);
    let bonds = asset.bonds.len() as u32;
    // A far seed turns the relevance filter on. A Define that arrives while `held` is still
    // `None` is broadcast, so the large structure has to be created after the filter is up.
    let seed = dense_asset(2);
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 44, setup_relevant);
    schedule(&mut hub.server);
    let client = join(&mut hub);
    {
        let mut net = hub.clients[client].world.resource_mut::<DestructionNet>();
        net.register_asset(asset.clone());
        net.register_asset(seed.clone());
    }
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.register_asset(asset.clone());
        net.register_asset(seed.clone());
    }
    hub.run(4);
    let seed_id = hub
        .server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(seed.clone(), DestructiblePose::at([1000.0, 0.0, 0.0]), 1)
        .expect("seed");
    let far = anchor(&mut hub, 1000.0);
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .set_relevance_anchor(seed_id, far);
    let view = viewpoint(&mut hub, client, 500.0);
    hub.run(8);
    assert!(
        stats_of(&hub.server).relevance_peers >= 1,
        "the relevance filter is on: {:?}",
        stats_of(&hub.server)
    );
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.config.snapshot_part_bytes = regia_framework::destruction_net::MIN_SNAPSHOT_PART_BYTES;
        net.config.snapshot_parts_per_tick = 1;
    }
    let id = {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        let id = d
            .world
            .define(asset.clone(), DestructiblePose::at([0.0, 0.0, 0.0]), 7)
            .expect("define");
        let mut snapshot = d.world.snapshot();
        let chain: std::collections::BTreeSet<u32> = asset
            .bonds
            .iter()
            .enumerate()
            .filter(|(_, b)| b.b == b.a + 1)
            .map(|(i, _)| i as u32)
            .collect();
        let row = snapshot
            .instances
            .iter_mut()
            .find(|row| row.id == id)
            .expect("the new instance");
        row.broken = (0..bonds).filter(|b| !chain.contains(b)).collect();
        assert!(
            row.broken.len() > 1_000,
            "the broken list has to exceed a 1 KiB part"
        );
        d.world.set_authority(Authority::Replica);
        let lookup = |which: DestructibleId| {
            if which == seed_id {
                Some(seed.clone())
            } else {
                Some(asset.clone())
            }
        };
        d.world.restore(&snapshot, &lookup).expect("restore");
        d.world.set_authority(Authority::Server);
        id
    };
    let near = anchor(&mut hub, 0.0);
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .set_relevance_anchor(id, near);
    hub.run(6);
    assert_eq!(
        stats_of(&hub.server).instances_gained,
        0,
        "{:?}",
        stats_of(&hub.server)
    );
    assert!(
        world_of(&hub.clients[client]).instance_snapshot(id).is_none(),
        "a far client does not receive the structure"
    );

    hub.server.world.entity_mut(view).insert(at_x(0.0));
    let mut saw_partial = false;
    let mut ticks = 0u32;
    let gained = loop {
        hub.step();
        ticks += 1;
        let server = stats_of(&hub.server);
        if server.split_instances >= 1 && server.instances_gained == 0 {
            saw_partial = true;
        }
        if server.instances_gained >= 1 || ticks > 80 {
            break server.instances_gained;
        }
    };
    assert!(
        saw_partial,
        "a split gain occupies a tick before it counts as gained"
    );
    assert_eq!(gained, 1);
    assert!(ticks > 1, "finished on tick {ticks}");
    hub.run(8);
    assert_eq!(
        world_of(&hub.clients[client]).instance_snapshot(id),
        world_of(&hub.server).instance_snapshot(id)
    );
    let client_stats = stats_of(&hub.clients[client]);
    assert_eq!(
        (client_stats.checks_failed, client_stats.resyncs_requested),
        (0, 0),
        "{client_stats:?}"
    );
}

/// Spatial relevance on the server: anchors and viewpoints within 10 m are held, nothing lingers.
fn setup_relevant(world: &mut World) {
    setup(world);
    if world.resource::<NetMode>().is_server() {
        regia_net::relevance::install_net_interest(world);
        let near = regia_net::profile::NetProfile {
            relevance: regia_net::profile::RelevanceShape::Spatial {
                radius: 10.0,
                hysteresis: 0.0,
            },
            linger_ticks: 0,
            ..regia_net::profile::NetProfile::default()
        };
        world
            .resource_mut::<regia_net::profile::NetProfiles>()
            .insert("near", near)
            .expect("profile");
    }
}

fn at_x(x: f32) -> regia_scene::Transform {
    regia_scene::Transform {
        translation: regia_core::math::Vec3::new(x, 0.0, 0.0),
        ..regia_scene::Transform::default()
    }
}

/// A replicated entity at `x` whose relevance an instance follows.
fn anchor(hub: &mut LoopbackHub, x: f32) -> Entity {
    hub.server
        .world
        .spawn((
            regia_net::replication::Replicated {
                transform: regia_net::replication::TransformReplication::None,
                ..regia_net::replication::Replicated::default()
            },
            at_x(x),
            regia_net::profile::NetProfileRef("near".into()),
        ))
        .id()
}

fn viewpoint(hub: &mut LoopbackHub, client: usize, x: f32) -> Entity {
    let peer = hub.clients[client].peer;
    hub.server
        .world
        .spawn((
            regia_net::relevance::NetViewpoint {
                player: regia_net::model::NetPlayerId { peer, local: 0 },
            },
            at_x(x),
        ))
        .id()
}

fn ids_of(p: &NetProcess) -> Vec<DestructibleId> {
    world_of(p).instances().map(|(id, _)| id).collect()
}

/// What a client holding `held` must hash to: the server's world over those instances.
fn held_hash(hub: &LoopbackHub, held: &[DestructibleId]) -> u64 {
    world_of(&hub.server).structure_hash_where(|id| held.contains(&id))
}

/// What a client holding only the wall hashes to after the relevance script (Linux x86_64,
/// 2026-11-02; Android aarch64 must match, ADR-2305).
const PINNED_WALL_ONLY_HASH: u64 = 0xba2c_34d5_796c_3547;
/// The same for a client holding only the bridge.
const PINNED_BRIDGE_ONLY_HASH: u64 = 0x806c_6df2_eefe_b2ed;

#[test]
fn each_peer_gets_only_the_structures_relevant_to_it_and_stays_checkable() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 41, setup_relevant);
    schedule(&mut hub.server);
    let a = join(&mut hub);
    let b = join(&mut hub);
    let ids = define(&mut hub);
    let (near_wall, near_bridge) = (anchor(&mut hub, 0.0), anchor(&mut hub, 100.0));
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.set_relevance_anchor(ids.wall, near_wall);
        net.set_relevance_anchor(ids.bridge, near_bridge);
    }
    let view_a = viewpoint(&mut hub, a, 0.0);
    viewpoint(&mut hub, b, 100.0);
    run_script(&mut hub, ids, 12);
    assert_eq!(world_of(&hub.server).structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(ids_of(&hub.clients[a]), vec![ids.wall]);
    assert_eq!(ids_of(&hub.clients[b]), vec![ids.bridge]);
    let before = stats_of(&hub.server);
    assert_eq!(before.relevance_peers, 2);
    assert!(before.breaks_skipped > 0, "{before:?}");
    // Both anchors had no NetId at the first tick, so each structure came by gaining it.
    assert_eq!((before.instances_gained, before.instances_lost), (2, 0));
    for (c, held) in [(a, ids.wall), (b, ids.bridge)] {
        let p = &hub.clients[c];
        let s = stats_of(p);
        assert_eq!((s.checks_failed, s.resyncs_requested), (0, 0), "{s:?}");
        assert!(s.checks_passed > 0);
        assert_eq!(s.last_problem, None);
        assert!(p.world.resource::<DestructionNet>().in_step(NetMode::Client));
        assert_eq!(world_of(p).structure_hash(), held_hash(&hub, &[held]));
        assert_eq!(world_of(p).event_count(), world_of(&hub.server).event_count());
        assert_eq!(
            world_of(p).instance_snapshot(held),
            world_of(&hub.server).instance_snapshot(held)
        );
        assert_eq!(
            hub.server.world.resource::<DestructionNet>().held_by(p.peer),
            Some(vec![held])
        );
    }
    println!(
        "DESTRUCTION_NET_RELEVANCE wall_only=0x{:016x} bridge_only=0x{:016x}",
        world_of(&hub.clients[a]).structure_hash(),
        world_of(&hub.clients[b]).structure_hash()
    );
    assert_eq!(world_of(&hub.clients[a]).structure_hash(), PINNED_WALL_ONLY_HASH);
    assert_eq!(
        world_of(&hub.clients[b]).structure_hash(),
        PINNED_BRIDGE_ONLY_HASH
    );

    // Peer a walks to the bridge: it loses the wall and gets the bridge as the server has it.
    hub.server.world.entity_mut(view_a).insert(at_x(100.0));
    hub.run(8);
    assert_eq!(ids_of(&hub.clients[a]), vec![ids.bridge]);
    let s = stats_of(&hub.clients[a]);
    assert_eq!((s.checks_failed, s.resyncs_requested), (0, 0), "{s:?}");
    assert_eq!(
        world_of(&hub.clients[a]).structure_hash(),
        PINNED_BRIDGE_ONLY_HASH
    );
    let after = stats_of(&hub.server);
    assert_eq!(
        (
            after.instances_lost - before.instances_lost,
            after.instances_gained - before.instances_gained
        ),
        (1, 1)
    );
    // Anchors cleared: both peers get the whole world back, without a snapshot.
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.clear_relevance_anchor(ids.wall);
        net.clear_relevance_anchor(ids.bridge);
    }
    hub.run(8);
    for c in [a, b] {
        let p = &hub.clients[c];
        assert_eq!(world_of(p).structure_hash(), PINNED_STRUCTURE_HASH);
        let s = stats_of(p);
        assert_eq!(
            (s.snapshots_applied, s.checks_failed, s.resyncs_requested),
            (1, 0, 0)
        );
    }
    assert_eq!(stats_of(&hub.server).relevance_peers, 0);
}

#[test]
fn a_structure_gained_mid_script_follows_the_stream_from_its_state() {
    // Peer a sees nothing at first; the wall becomes relevant halfway through the hits.
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 42, setup_relevant);
    schedule(&mut hub.server);
    let a = join(&mut hub);
    let ids = define(&mut hub);
    let near_wall = anchor(&mut hub, 0.0);
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .set_relevance_anchor(ids.wall, near_wall);
    let view = viewpoint(&mut hub, a, 500.0);
    for tick in 0..SAMPLE_TICKS {
        if tick == SAMPLE_TICKS / 2 {
            hub.server.world.entity_mut(view).insert(at_x(1.0));
        }
        hit_next_tick(&mut hub, ids);
        hub.step();
    }
    hub.run(12);
    assert_eq!(ids_of(&hub.clients[a]), vec![ids.wall, ids.bridge]);
    let s = stats_of(&hub.clients[a]);
    assert_eq!((s.checks_failed, s.resyncs_requested), (0, 0), "{s:?}");
    assert_eq!(world_of(&hub.clients[a]).structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(stats_of(&hub.server).instances_gained, 1);
}

#[test]
fn without_the_relevance_pipeline_an_anchor_filters_nothing() {
    let mut hub = hub(NetMode::DedicatedServer, 43, 2);
    let ids = define(&mut hub);
    let lonely = hub.server.world.spawn(at_x(1000.0)).id();
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .set_relevance_anchor(ids.wall, lonely);
    run_script(&mut hub, ids, 8);
    assert_eq!(stats_of(&hub.server).relevance_peers, 0);
    for c in &hub.clients {
        assert_eq!(world_of(c).structure_hash(), PINNED_STRUCTURE_HASH);
    }
}

/// Moves `id` on the server by one centimeter per call.
fn nudge(hub: &mut LoopbackHub, id: DestructibleId, x: f32, step: usize) {
    let pose = DestructiblePose::at([x + step as f32 * 0.01, 0.0, -5.0]);
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .set_pose(id, pose)
        .expect("pose");
}

#[test]
fn a_far_client_gets_no_pose_updates_for_an_irrelevant_instance() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 43, setup_relevant);
    schedule(&mut hub.server);
    let near = join(&mut hub);
    let far = join(&mut hub);
    let ids = define(&mut hub);
    let (wall_anchor, bridge_anchor) = (anchor(&mut hub, 0.0), anchor(&mut hub, 100.0));
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.set_relevance_anchor(ids.wall, wall_anchor);
        net.set_relevance_anchor(ids.bridge, bridge_anchor);
    }
    viewpoint(&mut hub, near, 0.0);
    let far_view = viewpoint(&mut hub, far, 100.0);
    hub.run(12);
    assert_eq!(ids_of(&hub.clients[near]), vec![ids.wall]);
    assert_eq!(ids_of(&hub.clients[far]), vec![ids.bridge]);

    // The wall moves: only the client that holds it hears about it, every tick and on refresh.
    for step in 0..40 {
        nudge(&mut hub, ids.wall, 0.0, step);
        hub.step();
    }
    hub.run(35);
    let (near_stats, far_stats, server) = (
        stats_of(&hub.clients[near]),
        stats_of(&hub.clients[far]),
        stats_of(&hub.server),
    );
    assert!(near_stats.poses_received >= 40, "{near_stats:?}");
    assert_eq!(far_stats.poses_received, 0, "{far_stats:?}");
    assert_eq!(server.poses_sent, near_stats.poses_received, "{server:?}");
    assert_eq!(server.poses_filtered, server.poses_sent, "{server:?}");
    assert_eq!(
        world_of(&hub.clients[near])
            .instance(ids.wall)
            .expect("wall")
            .pose(),
        world_of(&hub.server).instance(ids.wall).expect("wall").pose()
    );

    // The bridge moves too: each client hears only its own structure.
    let (near_before, far_before) = (near_stats.poses_received, far_stats.poses_received);
    for step in 0..40 {
        nudge(&mut hub, ids.bridge, 7.0, step);
        hub.step();
    }
    hub.run(2);
    assert!(stats_of(&hub.clients[far]).poses_received >= far_before + 40);
    assert_eq!(
        world_of(&hub.clients[far])
            .instance(ids.bridge)
            .expect("bridge")
            .pose(),
        world_of(&hub.server).instance(ids.bridge).expect("bridge").pose()
    );
    // The near client's count only moves for the wall's refreshes, never for the bridge's ticks.
    let refreshes = stats_of(&hub.clients[near]).poses_received - near_before;
    assert!(refreshes <= 2, "{refreshes} wall refreshes in 42 ticks");

    // Walking to the wall: the far client gains it in the pose it has now, with no pose message.
    let wall_pose = world_of(&hub.server).instance(ids.wall).expect("wall").pose();
    hub.server.world.entity_mut(far_view).insert(at_x(0.0));
    hub.run(6);
    let far_world = world_of(&hub.clients[far]);
    assert_eq!(far_world.instance(ids.wall).expect("gained").pose(), wall_pose);
    assert_eq!(stats_of(&hub.clients[far]).checks_failed, 0);
}

#[test]
fn with_no_anchor_every_client_gets_every_pose() {
    let mut hub = hub(NetMode::DedicatedServer, 44, 2);
    let ids = define(&mut hub);
    hub.run(4);
    for step in 0..10 {
        nudge(&mut hub, ids.wall, 0.0, step);
        hub.step();
    }
    hub.run(2);
    let server = stats_of(&hub.server);
    assert_eq!(server.poses_filtered, 0, "{server:?}");
    for c in &hub.clients {
        assert_eq!(stats_of(c).poses_received * 2, server.poses_sent, "{server:?}");
        assert_eq!(
            world_of(c).instance(ids.wall).expect("wall").pose(),
            world_of(&hub.server).instance(ids.wall).expect("wall").pose()
        );
    }
}

/// The split-gain rig of `a_relevance_gain_larger_than_the_byte_budget_arrives_over_several_ticks`:
/// relevance on, 1 KiB parts at one part per tick, a dense structure at x = 0 whose broken list
/// spans several parts, and a client viewpoint parked far away. Returns the hub, the client, its
/// viewpoint and the structure.
fn split_gain_rig(seed_value: u64) -> (LoopbackHub, usize, Entity, DestructibleId) {
    let (hub, client, view, ids) = split_gain_rig_n(seed_value, 1, 50);
    (hub, client, view, ids[0])
}

/// [`split_gain_rig`] with a 200-chunk structure: its gain takes about twenty ticks, so a
/// relevance flip or a resync lands while it is still under way.
fn split_gain_rig_long(seed_value: u64) -> (LoopbackHub, usize, Entity, DestructibleId) {
    let (hub, client, view, ids) = split_gain_rig_n(seed_value, 1, 200);
    (hub, client, view, ids[0])
}

/// [`split_gain_rig`] with `count` dense structures of `chunks` chunks side by side at x = 0, in
/// id order.
fn split_gain_rig_n(
    seed_value: u64,
    count: usize,
    chunks: u32,
) -> (LoopbackHub, usize, Entity, Vec<DestructibleId>) {
    let asset = dense_asset(chunks);
    let bonds = asset.bonds.len() as u32;
    let seed = dense_asset(2);
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed_value, setup_relevant);
    schedule(&mut hub.server);
    let client = join(&mut hub);
    for p in [&mut hub.clients[client].world, &mut hub.server.world] {
        let mut net = p.resource_mut::<DestructionNet>();
        net.register_asset(asset.clone());
        net.register_asset(seed.clone());
    }
    hub.run(4);
    let seed_id = hub
        .server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(seed.clone(), DestructiblePose::at([1000.0, 0.0, 0.0]), 1)
        .expect("seed");
    let far = anchor(&mut hub, 1000.0);
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .set_relevance_anchor(seed_id, far);
    let view = viewpoint(&mut hub, client, 500.0);
    hub.run(8);
    {
        let mut net = hub.server.world.resource_mut::<DestructionNet>();
        net.config.snapshot_part_bytes = regia_framework::destruction_net::MIN_SNAPSHOT_PART_BYTES;
        net.config.snapshot_parts_per_tick = 1;
    }
    let ids = {
        let mut d = hub.server.world.resource_mut::<Destruction>();
        let ids: Vec<DestructibleId> = (0..count)
            .map(|i| {
                d.world
                    .define(
                        asset.clone(),
                        DestructiblePose::at([0.0, 0.0, 2.0 * i as f32]),
                        7 + i as u64,
                    )
                    .expect("define")
            })
            .collect();
        let mut snapshot = d.world.snapshot();
        let chain: std::collections::BTreeSet<u32> = asset
            .bonds
            .iter()
            .enumerate()
            .filter(|(_, b)| b.b == b.a + 1)
            .map(|(i, _)| i as u32)
            .collect();
        for row in snapshot.instances.iter_mut().filter(|row| ids.contains(&row.id)) {
            row.broken = (0..bonds).filter(|b| !chain.contains(b)).collect();
        }
        d.world.set_authority(Authority::Replica);
        let lookup = |which: DestructibleId| {
            if which == seed_id {
                Some(seed.clone())
            } else {
                Some(asset.clone())
            }
        };
        d.world.restore(&snapshot, &lookup).expect("restore");
        d.world.set_authority(Authority::Server);
        ids
    };
    let near = anchor(&mut hub, 0.0);
    for &id in &ids {
        hub.server
            .world
            .resource_mut::<DestructionNet>()
            .set_relevance_anchor(id, near);
    }
    hub.run(6);
    assert_eq!(stats_of(&hub.server).instances_gained, 0);
    (hub, client, view, ids)
}

/// Moves the viewpoint next to the structure and runs until the split gain is under way: its
/// first piece is out, the rest is still queued.
fn start_split_gain(hub: &mut LoopbackHub, view: Entity) {
    hub.server.world.entity_mut(view).insert(at_x(0.0));
    for _ in 0..20 {
        hub.step();
        if stats_of(&hub.server).split_instances >= 1 {
            break;
        }
    }
    let server = stats_of(&hub.server);
    assert!(
        server.split_instances >= 1 && server.instances_gained == 0,
        "the gain is under way and not finished: {server:?}"
    );
}

/// corr-framework-a F2: a break on an instance whose split gain is still being sent reaches the
/// client after the gain, so the client ends where the server is without a failed check or a
/// resync (before the fix the break was skipped and the next Check failed).
#[test]
fn corr_fa_a_break_during_a_split_gain_reaches_the_client() {
    let (mut hub, client, view, id) = split_gain_rig(45);
    start_split_gain(&mut hub, view);
    let before = world_of(&hub.server).instance_snapshot(id);
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 49,
                impulse: [0.0, 1.0e15, 0.0],
            },
        })
        .expect("hit");
    hub.run(60);
    assert_ne!(
        world_of(&hub.server).instance_snapshot(id),
        before,
        "the hit broke the structure on the server"
    );
    assert_eq!(stats_of(&hub.server).instances_gained, 1);
    assert_eq!(
        world_of(&hub.clients[client]).instance_snapshot(id),
        world_of(&hub.server).instance_snapshot(id)
    );
    let c = stats_of(&hub.clients[client]);
    assert_eq!((c.checks_failed, c.resyncs_requested), (0, 0), "{c:?}");
    assert_eq!(stats_of(&hub.server).resyncs_served, 0);
}

/// corr-framework-a F2: the viewpoint leaves while a split gain is half sent and comes back. The
/// client's half-joined instance must not break the stream (before the fix the next piece failed
/// to join: `destruction_bad_snapshot` and a full resync).
#[test]
fn corr_fa_an_abandoned_split_gain_does_not_break_the_stream() {
    let (mut hub, client, view, id) = split_gain_rig_long(46);
    start_split_gain(&mut hub, view);
    hub.server.world.entity_mut(view).insert(at_x(500.0));
    hub.run(40);
    hub.server.world.entity_mut(view).insert(at_x(0.0));
    hub.run(120);
    assert_eq!(
        world_of(&hub.clients[client]).instance_snapshot(id),
        world_of(&hub.server).instance_snapshot(id)
    );
    let c = stats_of(&hub.clients[client]);
    assert_eq!(c.last_problem, None, "{c:?}");
    assert_eq!((c.checks_failed, c.resyncs_requested), (0, 0), "{c:?}");
}

/// corr-framework-a F2: a snapshot that starts while a split gain is half sent replaces it. The
/// gain's leftover pieces must not follow the snapshot (before the fix they did, the client
/// restored a fragment of the structure and the next Check failed into a second resync).
#[test]
fn corr_fa_a_snapshot_during_a_split_gain_drops_the_gain() {
    let (mut hub, client, view, id) = split_gain_rig_long(47);
    // Serve the resync at once (the default cooldown after the join snapshot would let the gain
    // finish first).
    hub.server
        .world
        .resource_mut::<DestructionNet>()
        .config
        .resync_cooldown_ticks = 0;
    start_split_gain(&mut hub, view);
    hub.clients[client]
        .world
        .resource_mut::<DestructionNet>()
        .request_resync();
    hub.run(150);
    assert_eq!(
        world_of(&hub.clients[client]).instance_snapshot(id),
        world_of(&hub.server).instance_snapshot(id)
    );
    let c = stats_of(&hub.clients[client]);
    assert_eq!(c.checks_failed, 0, "{c:?}");
    assert_eq!(
        stats_of(&hub.server).resyncs_served,
        1,
        "{:?}",
        stats_of(&hub.server)
    );
}

/// corr-framework-a F2: two structures gain one after the other, and the second breaks while the
/// first is still arriving. The second gain must start only once the peer's stream has caught up:
/// its pieces are cut from the world now, so the break still queued for the peer would apply twice.
#[test]
fn corr_fa_a_second_split_gain_waits_for_the_stream_to_catch_up() {
    let (mut hub, client, view, ids) = split_gain_rig_n(48, 2, 50);
    start_split_gain(&mut hub, view);
    let second = ids[1];
    let before = world_of(&hub.server).instance_snapshot(second);
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(second),
            kind: DamageKind::Impulse {
                chunk: 49,
                impulse: [0.0, 1.0e15, 0.0],
            },
        })
        .expect("hit");
    hub.run(120);
    assert_ne!(
        world_of(&hub.server).instance_snapshot(second),
        before,
        "the hit broke it"
    );
    assert_eq!(stats_of(&hub.server).instances_gained, 2);
    for &id in &ids {
        assert_eq!(
            world_of(&hub.clients[client]).instance_snapshot(id),
            world_of(&hub.server).instance_snapshot(id),
            "{id:?}"
        );
    }
    let c = stats_of(&hub.clients[client]);
    assert_eq!(c.last_problem, None, "{c:?}");
    assert_eq!((c.checks_failed, c.resyncs_requested), (0, 0), "{c:?}");
}
