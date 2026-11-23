// SPDX-License-Identifier: MPL-2.0
//! The replica side of the break model (ADR-1967): short events rebuild bit for bit, a snapshot
//! gives a late joiner the server's hash at any tick, and bad input changes nothing.

use std::sync::Arc;

use regia_physics::destruction::sample::{
    SAMPLE_HZ, SAMPLE_TICKS, define_sample, sample_assets, sample_hits,
};
use regia_physics::destruction::{
    Authority, BreakKind, CompactBreak, DestructibleAsset, DestructibleId, DestructiblePose,
    DestructionError, DestructionWorld,
};

/// Runs the sample on a server, calling `each` after every tick.
fn run_server(mut each: impl FnMut(&DestructionWorld, &[regia_physics::destruction::BreakEvent])) {
    let mut world = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut world).expect("define");
    for tick in 1..=SAMPLE_TICKS {
        for hit in sample_hits(tick, ids) {
            world.damage(hit).expect("hit");
        }
        let report = world.step(1.0 / SAMPLE_HZ);
        each(&world, &report.events);
    }
}

fn assets_for(world: &DestructionWorld) -> impl Fn(DestructibleId) -> Option<Arc<DestructibleAsset>> + use<> {
    let map: std::collections::BTreeMap<_, _> =
        world.instances().map(|(id, i)| (id, i.asset().clone())).collect();
    move |id| map.get(&id).cloned()
}

fn replica_of(world: &DestructionWorld) -> DestructionWorld {
    let mut replica = DestructionWorld::new(Authority::Replica);
    replica
        .restore(&world.snapshot(), &assets_for(world))
        .expect("restore");
    replica
}

#[test]
fn every_event_of_the_sample_compacts_and_rebuilds_bit_for_bit() {
    let mut count = 0;
    run_server(|world, events| {
        for event in events {
            let short = world.compact_event(event).expect("nothing moved");
            let back = world
                .expand_event(event.seq, event.tick, event.destructible, &short)
                .expect("rebuilds");
            assert_eq!(back, *event);
            count += 1;
        }
    });
    assert!(count > 100, "{count}");
}

#[test]
fn a_structure_that_moved_after_the_decision_does_not_compact() {
    let mut world = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut world).expect("define");
    let mut event = None;
    for tick in 1..=SAMPLE_TICKS {
        for hit in sample_hits(tick, ids) {
            world.damage(hit).expect("hit");
        }
        event = world
            .step(1.0 / SAMPLE_HZ)
            .events
            .into_iter()
            .find(|e| e.destructible == ids.wall && matches!(e.kind, BreakKind::ChunkDetached { .. }));
        if event.is_some() {
            break;
        }
    }
    let event = event.expect("a wall chunk detached");
    assert!(world.compact_event(&event).is_some());
    world
        .set_pose(ids.wall, DestructiblePose::at([1.0, 2.0, 3.0]))
        .expect("pose");
    assert_eq!(
        world.compact_event(&event),
        None,
        "the position no longer rebuilds"
    );
}

#[test]
fn expand_names_nothing_it_does_not_have() {
    let world = DestructionWorld::new(Authority::Replica);
    let bond = CompactBreak::Bond { bond: 0 };
    assert_eq!(world.expand_event(0, 1, DestructibleId(0), &bond), None);
    let mut world = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut world).expect("define");
    let far = CompactBreak::Bond { bond: 1_000_000 };
    assert_eq!(world.expand_event(0, 1, ids.wall, &far), None);
    let chunk = CompactBreak::Detach {
        chunk: 1_000_000,
        cause: regia_physics::destruction::DetachCause::Damage,
        linvel: [0.0; 3],
    };
    assert_eq!(world.expand_event(0, 1, ids.wall, &chunk), None);
}

#[test]
fn a_snapshot_at_any_tick_gives_the_servers_hash_and_the_events_after_it_apply() {
    let mut tails: Vec<(DestructionWorld, u64)> = Vec::new();
    let mut last = 0;
    run_server(|world, events| {
        for (replica, _) in &mut tails {
            replica.apply_events(events).expect("the stream continues");
            assert_eq!(replica.structure_hash(), world.structure_hash());
        }
        let replica = replica_of(world);
        assert_eq!(replica.structure_hash(), world.structure_hash());
        assert_eq!(replica.event_count(), world.event_count());
        assert_eq!(replica.tick(), world.tick());
        assert_eq!(replica.pending_spawns(), 0, "what fell before is gone");
        for (id, inst) in world.instances() {
            let r = replica.instance(id).expect("instance");
            assert_eq!(r.chunk_states(), inst.chunk_states());
            assert_eq!(r.bond_broken(), inst.bond_broken());
        }
        tails.push((replica, world.tick()));
        last = world.tick();
    });
    assert_eq!(last, SAMPLE_TICKS);
    assert_eq!(tails.len() as u64, SAMPLE_TICKS);
}

#[test]
fn a_snapshot_lists_only_what_a_detached_chunk_does_not_imply() {
    let mut last = None;
    run_server(|world, _| last = Some(world.snapshot()));
    let snapshot = last.expect("ran");
    let wall = &snapshot.instances[0];
    assert!(!wall.detached.is_empty());
    assert!(wall.detached.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert!(wall.broken.windows(2).all(|w| w[0] < w[1]), "ascending");
}

#[test]
fn a_bad_snapshot_changes_nothing() {
    let mut server = DestructionWorld::new(Authority::Server);
    define_sample(&mut server).expect("define");
    let assets = assets_for(&server);
    let mut replica = replica_of(&server);
    let before = replica.structure_hash();
    let good = server.snapshot();
    let bad_chunk = {
        let mut s = good.clone();
        s.instances[0].detached = vec![1_000_000];
        s
    };
    let bad_bond = {
        let mut s = good.clone();
        s.instances[0].broken = vec![1_000_000];
        s
    };
    let doubled = {
        let mut s = good.clone();
        let first = s.instances[0].clone();
        s.instances.push(first);
        s
    };
    let bad_pose = {
        let mut s = good.clone();
        s.instances[0].pose.rotation = [0.0; 4];
        s
    };
    for snapshot in [&bad_chunk, &bad_bond, &doubled] {
        let e = replica.restore(snapshot, &assets).map_err(|e| e.code());
        assert_eq!(e, Err("destruction_bad_snapshot"));
        assert_eq!(replica.structure_hash(), before);
    }
    let e = replica.restore(&bad_pose, &assets).map_err(|e| e.code());
    assert_eq!(e, Err("destruction_invalid_pose"));
    let e = replica.restore(&good, &|_| None).map_err(|e| e.code());
    assert_eq!(e, Err("destruction_bad_snapshot"));
    assert_eq!(replica.structure_hash(), before);
    let e = server.restore(&good, &assets).map_err(|e| e.code());
    assert_eq!(e, Err("destruction_not_replica"));
}

#[test]
fn define_at_is_for_replicas_and_keeps_ids_clear() {
    let (wall, _) = sample_assets().expect("assets");
    let mut server = DestructionWorld::new(Authority::Server);
    assert_eq!(
        server.define_at(DestructibleId(3), wall.clone(), DestructiblePose::default(), 1),
        Err(DestructionError::NotReplica)
    );
    let mut replica = DestructionWorld::new(Authority::Replica);
    replica
        .define_at(DestructibleId(3), wall.clone(), DestructiblePose::default(), 1)
        .expect("define");
    let taken = replica
        .define_at(DestructibleId(3), wall.clone(), DestructiblePose::default(), 1)
        .map_err(|e| e.code());
    assert_eq!(taken, Err("destruction_bad_snapshot"));
    let next = replica
        .define(wall.clone(), DestructiblePose::default(), 1)
        .expect("define");
    assert_eq!(next, DestructibleId(4), "ids run on past the highest");
    let zero = DestructiblePose {
        translation: [0.0; 3],
        rotation: [0.0; 4],
    };
    let e = replica
        .define_at(DestructibleId(9), wall, zero, 1)
        .map_err(|e| e.code());
    assert_eq!(e, Err("destruction_invalid_pose"));
}

#[test]
fn a_replica_keeps_the_servers_rotation_bit_for_bit() {
    let (wall, _) = sample_assets().expect("assets");
    let mut moved_by_second_normalization = 0;
    for i in 1..200u32 {
        let a = i as f32 * 0.137;
        let q = [
            (a * 0.9).sin(),
            (a * 1.7).cos(),
            (a * 0.3).sin(),
            (a * 2.1).cos() + 1.5,
        ];
        let mut server = DestructionWorld::new(Authority::Server);
        let pose = DestructiblePose {
            translation: [1.0, 2.0, 3.0],
            rotation: q,
        };
        let id = server.define(wall.clone(), pose, 1).expect("define");
        let exact = server.instance(id).expect("instance").pose();
        // Defining the normalized pose again on a server normalizes a second time.
        let twice = server.define(wall.clone(), exact, 1).expect("define");
        if server.instance(twice).expect("instance").pose() != exact {
            moved_by_second_normalization += 1;
        }
        let mut replica = DestructionWorld::new(Authority::Replica);
        replica.define_at(id, wall.clone(), exact, 1).expect("define");
        assert_eq!(replica.instance(id).expect("instance").pose(), exact);
        let restored = replica_of(&server);
        assert_eq!(restored.instance(id).expect("instance").pose(), exact);
    }
    assert!(
        moved_by_second_normalization > 0,
        "the case this guards against must happen in this sample of rotations"
    );
}

#[test]
fn a_non_finite_event_is_refused_before_it_reaches_rapier() {
    let mut server = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut server).expect("define");
    let mut replica = replica_of(&server);
    let mut event = None;
    for tick in 1..=SAMPLE_TICKS {
        for hit in sample_hits(tick, ids) {
            server.damage(hit).expect("hit");
        }
        event = server.step(1.0 / SAMPLE_HZ).events.into_iter().next();
        if event.is_some() {
            break;
        }
    }
    let mut event = event.expect("a break");
    match &mut event.kind {
        BreakKind::BondBroken { position, .. } | BreakKind::ChunkDetached { position, .. } => {
            position[0] = f32::NAN;
        }
    }
    let e = replica.apply_events(&[event]).map_err(|e| e.code());
    assert_eq!(e, Err("destruction_bad_event"));
    assert_eq!(replica.event_count(), 0);
}

#[test]
fn becoming_a_replica_forgets_queued_hits() {
    let mut world = DestructionWorld::new(Authority::Server);
    let ids = define_sample(&mut world).expect("define");
    for tick in 1..=SAMPLE_TICKS {
        for hit in sample_hits(tick, ids) {
            world.damage(hit).expect("hit");
        }
    }
    assert!(world.queued_damage() > 0);
    world.set_authority(Authority::Replica);
    assert_eq!(world.queued_damage(), 0);
    assert_eq!(world.authority(), Authority::Replica);
    world.set_authority(Authority::Server);
    assert_eq!(world.authority(), Authority::Server);
}

#[test]
fn an_empty_snapshot_clears_a_replica() {
    let server = DestructionWorld::new(Authority::Server);
    let mut other = DestructionWorld::new(Authority::Server);
    define_sample(&mut other).expect("define");
    let mut replica = replica_of(&other);
    assert!(replica.instances().next().is_some());
    replica.restore(&server.snapshot(), &|_| None).expect("restore");
    assert!(replica.instances().next().is_none());
    assert_eq!(replica.structure_hash(), server.structure_hash());
}
