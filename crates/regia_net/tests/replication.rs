// SPDX-License-Identifier: MPL-2.0
//! Spec 71 § 14: entity replication over the in-memory `LoopbackHub` at fixed steps.

use glam::Vec3;
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_net::codec::layout_hash;
use regia_net::replication::{
    NetOutOfScope, NetPredicted, NetPrefabSpawner, NetRelevance, NetTeleported, NetUnbound, NetVecEventKind,
    ServerReplication, register_netvec, register_replicated,
};
use regia_net::runtime::{NetEntities, protocol_hash};
use regia_net::testing::{LinkConditions, LoopbackHub, every_test_on_state_channel};
use regia_net::*;
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use regia_scene::prefab::PrefabInstance;
use regia_scene::transform::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Stats {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    e: u32,
    f: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Pulse {
    n: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Quiet {
    n: u32,
}

macro_rules! audience_component {
    ($($name:ident),*) => {$(
        #[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
        struct $name(u32);
    )*};
}
audience_component!(
    OwnerSecret,
    OthersView,
    SpawnOnlyC,
    SpawnThenOwnerC,
    ProxyC,
    GroupC,
    PredC
);

#[derive(Component)]
struct Flag;

fn flagged(world: &World, e: Entity) -> bool {
    world.get::<Flag>(e).is_some()
}

#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Target(Entity);

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Item {
    count: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Aim {
    #[replicate(quantize(min = -10.0, max = 10.0, bits = 10))]
    x: f32,
}

fn opts(audience: Audience) -> ReplicateOptions {
    ReplicateOptions {
        audience,
        ..ReplicateOptions::default()
    }
}

fn setup(world: &mut World) {
    register_replicated::<Stats>(world, ReplicateOptions::default());
    register_replicated::<Health>(
        world,
        ReplicateOptions {
            keep_previous: true,
            notify_on_server: true,
            ..ReplicateOptions::default()
        },
    );
    register_replicated::<Pulse>(
        world,
        ReplicateOptions {
            notify: NotifyWhen::EveryUpdate,
            ..ReplicateOptions::default()
        },
    );
    register_replicated::<Quiet>(
        world,
        ReplicateOptions {
            notify: NotifyWhen::Never,
            ..ReplicateOptions::default()
        },
    );
    register_replicated::<OwnerSecret>(world, opts(Audience::OwnerOnly));
    register_replicated::<OthersView>(world, opts(Audience::Others));
    register_replicated::<SpawnOnlyC>(world, opts(Audience::SpawnOnly));
    register_replicated::<SpawnThenOwnerC>(world, opts(Audience::SpawnThenOwner));
    register_replicated::<ProxyC>(world, opts(Audience::ProxyCopies));
    register_replicated::<GroupC>(world, opts(Audience::SharedGroup));
    register_replicated::<PredC>(world, opts(Audience::Predicate(flagged)));
    register_replicated::<Target>(world, ReplicateOptions::default());
    register_replicated::<Aim>(world, ReplicateOptions::default());
    register_netvec::<Item>(world, Audience::All);
}

fn hub(seed: u64) -> LoopbackHub {
    LoopbackHub::new(NetMode::DedicatedServer, seed, setup)
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn id_of(hub: &LoopbackHub, e: Entity) -> NetId {
    *hub.server
        .world
        .get::<NetId>(e)
        .expect("replicated entity has a NetId")
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn get<C: Component + Clone>(hub: &LoopbackHub, client: usize, e: Entity) -> Option<C> {
    let c = copy(hub, client, e)?;
    hub.clients[client].world.get::<C>(c).cloned()
}

/// § 14 test 1.
#[test]
fn opt_in_spawn_and_despawn() {
    let mut hub = hub(1);
    let a = hub.add_client();
    let _local = hub
        .server
        .world
        .spawn(Stats {
            a: 1,
            ..Stats::default()
        })
        .id();
    let net = hub
        .server
        .world
        .spawn((
            rep(),
            Stats {
                a: 2,
                ..Stats::default()
            },
        ))
        .id();
    hub.run(3);
    let w = &hub.clients[a].world;
    assert_eq!(
        w.resource::<NetEntities>().len(),
        1,
        "only the Replicated entity travels"
    );
    assert_eq!(get::<Stats>(&hub, a, net).map(|s| s.a), Some(2));
    let c = copy(&hub, a, net).expect("copy");
    assert_eq!(hub.clients[a].world.get::<NetRole>(c), Some(&NetRole::Proxy));
    assert!(hub.server.world.despawn(net));
    hub.run(3);
    assert!(
        hub.clients[a].world.get_entity(c).is_err(),
        "despawn reaches the copy"
    );
    assert!(hub.clients[a].world.resource::<NetEntities>().is_empty());
}

#[derive(Component)]
struct PrefabMarker;
#[derive(Component)]
struct PrefabChild;

/// § 14 test 2.
#[test]
fn prefab_copies_instantiate_locally() {
    let mut hub = hub(2);
    let a = hub.add_client();
    hub.clients[a]
        .world
        .insert_resource(NetPrefabSpawner(Box::new(|world: &mut World, prefab: u128| {
            if prefab != 42 {
                return None;
            }
            let root = world.spawn(PrefabMarker).id();
            world.spawn((PrefabChild, ChildOf(root)));
            Some(root)
        })));
    let inst = |prefab| PrefabInstance {
        prefab,
        ..PrefabInstance::default()
    };
    let good = hub.server.world.spawn((rep(), inst(42), Health { hp: 7 })).id();
    let missing1 = hub.server.world.spawn((rep(), inst(43), Health { hp: 8 })).id();
    let missing2 = hub.server.world.spawn((rep(), inst(43), Health { hp: 9 })).id();
    hub.run(3);
    let c = copy(&hub, a, good).expect("copy");
    let w = &hub.clients[a].world;
    assert!(w.get::<PrefabMarker>(c).is_some());
    assert!(
        w.get::<Children>(c).is_some_and(|ch| ch.len() == 1),
        "non-replicated parts come from the prefab"
    );
    assert_eq!(
        w.get::<Health>(c),
        Some(&Health { hp: 7 }),
        "replicated values on top"
    );
    for m in [missing1, missing2] {
        let mc = copy(&hub, a, m).expect("bare copy");
        assert!(w.get::<PrefabMarker>(mc).is_none());
    }
    let client = w.resource::<regia_net::replication::ClientReplication>();
    assert_eq!(client.counters.prefab_missing, 2);
}

/// The newest frame that carries state, on either wire: tick frames or state frames.
fn last_tick_frame_len(hub: &LoopbackHub, client: usize) -> usize {
    hub.client_logs[client]
        .iter()
        .rev()
        .find(|(_, d)| matches!(d.bytes.first(), Some(1 | 7)))
        .map(|(_, d)| d.bytes.len())
        .expect("a tick frame")
}

fn max_frame_since(hub: &LoopbackHub, client: usize, from: usize) -> usize {
    hub.client_logs[client][from..]
        .iter()
        .filter(|(_, d)| matches!(d.bytes.first(), Some(1 | 7)))
        .map(|(_, d)| d.bytes.len())
        .max()
        .expect("frames")
}

/// § 14 test 3: one changed field of six sends that field only; writing the same value sends
/// nothing.
#[test]
fn field_deltas_send_changed_fields_only() {
    let mut hub = hub(3);
    let a = hub.add_client();
    let e = hub.server.world.spawn((rep(), Stats::default())).id();
    hub.run(5);
    let empty = last_tick_frame_len(&hub, a);
    let mark = hub.client_logs[a].len();
    hub.server.world.get_mut::<Stats>(e).expect("stats").c = 1000;
    hub.run(2);
    let one = max_frame_since(&hub, a, mark);
    hub.run(2);
    let mark = hub.client_logs[a].len();
    {
        let mut s = hub.server.world.get_mut::<Stats>(e).expect("stats");
        s.a = 1000;
        s.b = 1000;
        s.d = 1000;
        s.e = 1000;
        s.f = 1000;
        s.c = 2000;
    }
    hub.step();
    hub.step();
    let six = max_frame_since(&hub, a, mark);
    assert!(one > empty, "{one} > {empty}");
    assert!(six >= one + 8, "six fields cost more than one: {six} vs {one}");
    // One field: the entity id, a few headers, 6 change bits and a 2-byte varint.
    // A state record also names its id, length and version offsets.
    let framing = if every_test_on_state_channel() { 3 } else { 0 };
    assert!(
        one - empty <= 10 + framing,
        "one field delta is small: {}",
        one - empty
    );
    hub.run(2);
    let settled = last_tick_frame_len(&hub, a);
    // Same value written: a compare, no bandwidth.
    hub.server.world.get_mut::<Stats>(e).expect("stats").c = 2000;
    hub.step();
    hub.step();
    assert_eq!(last_tick_frame_len(&hub, a), settled);
    assert_eq!(settled, empty);
    assert_eq!(get::<Stats>(&hub, a, e).map(|s| (s.a, s.c)), Some((1000, 2000)));
}

fn fnv(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// § 14 test 4: half a step is no change; out-of-range values clamp and count; the encodings of
/// 1,000 deterministic inputs hash to a fixed value (the same on every platform: integer
/// arithmetic after one multiply-and-round).
#[test]
fn quantization_is_stable_and_counts_clamps() {
    use regia_net::codec::{Angle, CodecContext, FieldCodec, Quantize, Rotation, UnitVec, encode_to_vec};
    let cx = CodecContext::detached();
    let step = 20.0 / 1023.0;
    assert!(Aim { x: 1.0 }.wire_eq(&Aim { x: 1.0 + step * 0.45 }, &cx));
    assert!(!Aim { x: 1.0 }.wire_eq(&Aim { x: 1.0 + step * 1.5 }, &cx));

    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut rng = regia_net::testing::SplitMix64::new(71);
    let q = Quantize {
        min: -100.0,
        max: 100.0,
        bits: 12,
    };
    for _ in 0..1000 {
        let v = (rng.next_f64() * 300.0 - 150.0) as f32;
        let mut w = BitWriter::new();
        FieldCodec::<f32>::encode(&q, &v, &mut w, &cx);
        FieldCodec::<f32>::encode(&Angle { bits: 11 }, &v, &mut w, &cx);
        // Inputs come from the integer generator only (no transcendental functions), so they are
        // bit-identical on every platform; the codecs need no unit-length input.
        let mut r = || (rng.next_f64() * 2.0 - 1.0) as f32;
        let dir = Vec3::new(v, r(), r());
        FieldCodec::<Vec3>::encode(&UnitVec { bits: 10 }, &dir, &mut w, &cx);
        let rot = glam::Quat::from_xyzw(r(), r(), r(), r());
        FieldCodec::<glam::Quat>::encode(&Rotation::default(), &rot, &mut w, &cx);
        h = fnv(&w.finish(), h);
        h = fnv(&encode_to_vec(&Aim { x: v / 15.0 }, &cx), h);
    }
    assert!(
        cx.take_stats().clamped > 0,
        "out-of-range inputs were clamped and counted"
    );
    assert_eq!(h, GOLDEN_QUANT_HASH, "wire bytes changed: {h:#018x}");

    // Clamps on a replicated component are counted by the server.
    let mut hub = hub(4);
    hub.add_client();
    hub.server.world.spawn((rep(), Aim { x: 50.0 }));
    hub.run(3);
    assert!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .quantize_clamped
            >= 1
    );
}

/// FNV-1a of the 1,000-input encodings (Linux x86_64 reference, the build server, 2026-10-29).
const GOLDEN_QUANT_HASH: u64 = 0xd076_3481_7699_8b24;

/// § 14 test 5: one test per audience, with an owner (predicting) and two other clients, and a
/// shared-group membership change mid-session.
#[test]
fn audiences() {
    let mut hub = hub(5);
    let a = hub.add_client();
    let b = hub.add_client();
    let c = hub.add_client();
    let (pa, pb, pc) = (hub.clients[a].peer, hub.clients[b].peer, hub.clients[c].peer);
    let player = |peer: PeerId| {
        (
            NetPlayer {
                id: NetPlayerId { peer, local: 0 },
            },
            NetGroups(vec!["red".into()]),
        )
    };
    hub.server.world.spawn(player(pb));
    let e = hub
        .server
        .world
        .spawn((
            rep(),
            NetOwner(pa),
            NetPredicted,
            NetGroups(vec!["red".into()]),
            Flag,
            (OwnerSecret(1), OthersView(1), SpawnOnlyC(1), SpawnThenOwnerC(1)),
            (ProxyC(1), GroupC(1), PredC(1)),
        ))
        .id();
    hub.run(3);
    let has = |hub: &LoopbackHub, i: usize, f: &dyn Fn(&World, Entity) -> bool| {
        let ce = copy(hub, i, e).expect("copy");
        f(&hub.clients[i].world, ce)
    };
    let row = |hub: &LoopbackHub, f: &dyn Fn(&World, Entity) -> bool| {
        [has(hub, a, f), has(hub, b, f), has(hub, c, f)]
    };
    assert_eq!(
        row(&hub, &|w, x| w.get::<OwnerSecret>(x).is_some()),
        [true, false, false]
    );
    assert_eq!(
        row(&hub, &|w, x| w.get::<OthersView>(x).is_some()),
        [false, true, true]
    );
    assert_eq!(
        row(&hub, &|w, x| w.get::<SpawnOnlyC>(x).is_some()),
        [true, true, true]
    );
    assert_eq!(
        row(&hub, &|w, x| w.get::<SpawnThenOwnerC>(x).is_some()),
        [true, true, true]
    );
    assert_eq!(
        row(&hub, &|w, x| w.get::<ProxyC>(x).is_some()),
        [false, true, true]
    );
    assert_eq!(
        row(&hub, &|w, x| w.get::<GroupC>(x).is_some()),
        [false, true, false]
    );
    assert_eq!(row(&hub, &|w, x| w.get::<PredC>(x).is_some()), [true, true, true]);
    assert_eq!(
        hub.clients[a]
            .world
            .get::<NetRole>(copy(&hub, a, e).expect("copy")),
        Some(&NetRole::Predicted)
    );

    // Later changes.
    hub.server.world.entity_mut(e).remove::<Flag>();
    {
        let mut ent = hub.server.world.entity_mut(e);
        ent.get_mut::<SpawnOnlyC>().expect("c").0 = 2;
        ent.get_mut::<SpawnThenOwnerC>().expect("c").0 = 2;
        ent.get_mut::<PredC>().expect("c").0 = 2;
        ent.get_mut::<GroupC>().expect("c").0 = 2;
    }
    hub.run(3);
    let val = |hub: &LoopbackHub, i: usize, f: &dyn Fn(&World, Entity) -> Option<u32>| {
        let ce = copy(hub, i, e).expect("copy");
        f(&hub.clients[i].world, ce)
    };
    let spawn_only = |w: &World, x| w.get::<SpawnOnlyC>(x).map(|v| v.0);
    let spawn_then_owner = |w: &World, x| w.get::<SpawnThenOwnerC>(x).map(|v| v.0);
    let pred = |w: &World, x| w.get::<PredC>(x).map(|v| v.0);
    let group = |w: &World, x| w.get::<GroupC>(x).map(|v| v.0);
    for i in [a, b, c] {
        assert_eq!(val(&hub, i, &spawn_only), Some(1), "spawn-only never updates");
        assert_eq!(val(&hub, i, &pred), Some(1), "predicate false: no updates");
    }
    assert_eq!(val(&hub, a, &spawn_then_owner), Some(2));
    assert_eq!(val(&hub, b, &spawn_then_owner), Some(1));
    assert_eq!(val(&hub, b, &group), Some(2));
    assert_eq!(val(&hub, c, &group), None);
    // c joins the group mid-session: it receives the full current value once.
    hub.server.world.spawn(player(pc));
    hub.run(3);
    assert_eq!(val(&hub, c, &group), Some(2));
}

#[derive(Resource, Default, Debug)]
struct Notices {
    health: Vec<Option<u32>>,
    pulse: u32,
    quiet: u32,
}

fn collect_notices(
    h: Option<Res<NetChangedEvents<Health>>>,
    p: Option<Res<NetChangedEvents<Pulse>>>,
    q: Option<Res<NetChangedEvents<Quiet>>>,
    mut n: ResMut<Notices>,
) {
    if let Some(h) = h {
        n.health
            .extend(h.iter().map(|c| c.previous.as_ref().map(|v| v.hp)));
    }
    if let Some(p) = p {
        n.pulse += p.len() as u32;
    }
    if let Some(q) = q {
        n.quiet += q.len() as u32;
    }
}

/// § 14 test 6.
#[test]
fn change_notices() {
    let mut hub = hub(6);
    let a = hub.add_client();
    for p in std::iter::once(&mut hub.server).chain(hub.clients.iter_mut()) {
        p.world.init_resource::<Notices>();
        p.gameplay.add_systems(collect_notices);
    }
    let e = hub
        .server
        .world
        .spawn((rep(), Health { hp: 100 }, Pulse { n: 1 }, Quiet { n: 1 }))
        .id();
    hub.run(3);
    {
        let n = hub.clients[a].world.resource::<Notices>();
        assert_eq!(n.health, vec![None], "spawn: one notice, no previous");
        assert_eq!(n.pulse, 1);
        assert_eq!(n.quiet, 0);
    }
    hub.server.world.get_mut::<Health>(e).expect("hp").hp = 50;
    hub.run(3);
    assert_eq!(
        hub.clients[a].world.resource::<Notices>().health,
        vec![None, Some(100)]
    );
    // Same value: nothing.
    hub.server.world.get_mut::<Health>(e).expect("hp").hp = 50;
    hub.run(3);
    assert_eq!(hub.clients[a].world.resource::<Notices>().health.len(), 2);
    // A resend delivers every component again: EveryUpdate fires, Changed and Never do not.
    net_resend(&mut hub.server.world, e);
    hub.run(3);
    let n = hub.clients[a].world.resource::<Notices>();
    assert_eq!(n.health.len(), 2);
    assert_eq!(n.pulse, 2);
    assert_eq!(n.quiet, 0);
    // notify_on_server: the server saw its own change (spawn value, then 100 -> 50).
    let s = hub.server.world.resource::<Notices>();
    assert_eq!(s.health, vec![None, Some(100)]);
}

#[derive(Resource, Default)]
struct Hidden(Vec<Entity>);

fn not_hidden(world: &World, e: Entity, _peer: PeerId) -> bool {
    world.get_resource::<Hidden>().is_none_or(|h| !h.0.contains(&e))
}

#[derive(Resource, Default)]
struct Resolved(u32);

fn count_resolved(ev: Res<NetEvents>, mut r: ResMut<Resolved>) {
    r.0 += ev.refs_resolved.len() as u32;
}

/// § 14 test 7.
#[test]
fn entity_references() {
    let mut hub = hub(7);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<Resolved>();
    hub.clients[a].gameplay.add_systems(count_resolved);
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(not_hidden),
    });
    let target = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    hub.server.world.insert_resource(Hidden(vec![target]));
    let referrer = hub.server.world.spawn((rep(), Target(target))).id();
    let local = hub.server.world.spawn(Health { hp: 2 }).id();
    let to_local = hub.server.world.spawn((rep(), Target(local))).id();
    hub.run(3);
    assert_eq!(
        get::<Target>(&hub, a, referrer),
        Some(Target(Entity::PLACEHOLDER)),
        "target not here yet"
    );
    assert_eq!(
        get::<Target>(&hub, a, to_local),
        Some(Target(Entity::PLACEHOLDER)),
        "not replicated: none"
    );
    let counters = hub.server.world.resource::<ServerReplication>().counters;
    assert!(counters.refs_not_replicated >= 1);
    {
        let client = hub.clients[a]
            .world
            .resource::<regia_net::replication::ClientReplication>();
        assert_eq!(client.pending_refs.len(), 1);
        assert_eq!(client.pending_refs[0].waiting_for, id_of(&hub, target));
    }
    // Five ticks later the target becomes relevant and spawns: the reference resolves.
    hub.run(5);
    hub.server.world.resource_mut::<Hidden>().0.clear();
    hub.run(6);
    let tc = copy(&hub, a, target).expect("target copy");
    assert_eq!(get::<Target>(&hub, a, referrer), Some(Target(tc)));
    assert_eq!(hub.clients[a].world.resource::<Resolved>().0, 1);
    // The target leaves relevance: the field becomes a pending reference again.
    hub.server.world.resource_mut::<Hidden>().0.push(target);
    hub.run(6);
    assert_eq!(
        get::<Target>(&hub, a, referrer),
        Some(Target(Entity::PLACEHOLDER))
    );
    let client = hub.clients[a]
        .world
        .resource::<regia_net::replication::ClientReplication>();
    assert_eq!(client.pending_refs.len(), 1);
}

/// § 14 test 8.
#[test]
fn hierarchy_parents_first_and_local_parents() {
    let mut hub = hub(8);
    let a = hub.add_client();
    let parent = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    let child = hub
        .server
        .world
        .spawn((rep(), Health { hp: 2 }, ChildOf(parent)))
        .id();
    let local_parent = hub.server.world.spawn(Health { hp: 3 }).id();
    let orphan = hub
        .server
        .world
        .spawn((rep(), Health { hp: 4 }, ChildOf(local_parent)))
        .id();
    hub.run(3);
    let pc = copy(&hub, a, parent).expect("parent copy");
    let cc = copy(&hub, a, child).expect("child copy");
    assert_eq!(hub.clients[a].world.get::<ChildOf>(cc).map(|c| c.0), Some(pc));
    let oc = copy(&hub, a, orphan).expect("orphan copy");
    assert!(
        hub.clients[a].world.get::<ChildOf>(oc).is_none(),
        "a root on the client"
    );
    let counters = hub.server.world.resource::<ServerReplication>().counters;
    assert_eq!(counters.parent_not_replicated, 1);
    hub.run(3);
    assert_eq!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .parent_not_replicated,
        1,
        "warned once"
    );
}

fn level_entity(world: &mut World, guid: u128, a: u32) -> Entity {
    world
        .spawn((
            rep(),
            LevelMember(7),
            EntityGuid(Guid::from_u128(guid)),
            Stats {
                a,
                ..Stats::default()
            },
        ))
        .id()
}

/// § 14 test 9.
#[test]
fn level_bound_entities() {
    let mut hub = hub(9);
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(not_hidden),
    });
    hub.server.world.init_resource::<Hidden>();
    let s1 = level_entity(&mut hub.server.world, 100, 1);
    let s2 = level_entity(&mut hub.server.world, 101, 1);
    let s3 = level_entity(&mut hub.server.world, 102, 1);
    hub.run(2);
    // A door blown off before anyone joined.
    assert!(hub.server.world.despawn(s2));
    hub.run(2);
    // The client loads the level (it lacks guid 102: a content mismatch).
    let a = hub.add_client();
    let c1 = level_entity(&mut hub.clients[a].world, 100, 0);
    let c2 = level_entity(&mut hub.clients[a].world, 101, 0);
    hub.step();
    {
        let w = &hub.clients[a].world;
        assert!(w.get::<NetUnbound>(c1).is_some(), "shown unbound before the bind");
        assert_eq!(w.get::<NetRole>(c1), Some(&NetRole::Proxy));
    }
    hub.run(2);
    let w = &hub.clients[a].world;
    assert_eq!(copy(&hub, a, s1), Some(c1), "bound to the client's own entity");
    assert!(w.get::<NetUnbound>(c1).is_none());
    assert_eq!(w.get::<Stats>(c1).map(|s| s.a), Some(1), "server state applied");
    assert!(w.get_entity(c2).is_err(), "removed before join: despawned");
    let bare = copy(&hub, a, s3).expect("missing guid spawns bare");
    assert!(w.get::<LevelMember>(bare).is_none());
    let client = w.resource::<regia_net::replication::ClientReplication>();
    assert_eq!(client.counters.level_entity_missing, 1);
    // Relevance loss keeps the copy, out of scope; regaining sends full state.
    hub.server.world.resource_mut::<Hidden>().0.push(s1);
    hub.run(3);
    assert!(hub.clients[a].world.get::<NetOutOfScope>(c1).is_some());
    hub.server.world.get_mut::<Stats>(s1).expect("stats").a = 9;
    hub.run(3);
    assert_eq!(
        hub.clients[a].world.get::<Stats>(c1).map(|s| s.a),
        Some(1),
        "out of scope: no updates"
    );
    hub.server.world.resource_mut::<Hidden>().0.clear();
    hub.run(3);
    let w = &hub.clients[a].world;
    assert!(w.get::<NetOutOfScope>(c1).is_none());
    assert_eq!(w.get::<Stats>(c1).map(|s| s.a), Some(9));
}

fn asleep(hub: &LoopbackHub, e: Entity) -> bool {
    hub.server
        .world
        .resource::<ServerReplication>()
        .is_asleep(id_of(hub, e))
        == Some(true)
}

/// § 14 test 10.
#[test]
fn sleep_rules() {
    let mut hub = hub(10);
    let a = hub.add_client();
    let start = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::StartAsleep,
                ..rep()
            },
            Health { hp: 1 },
        ))
        .id();
    let quiet = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::AfterQuiet { quiet_ticks: 5 },
                ..rep()
            },
            Health { hp: 1 },
        ))
        .id();
    let never = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    hub.run(4);
    assert!(asleep(&hub, start), "asleep once the spawn is acknowledged");
    assert!(!asleep(&hub, quiet));
    hub.run(4);
    assert!(asleep(&hub, quiet), "asleep after 5 quiet ticks");
    assert!(!asleep(&hub, never));
    // Asleep: a change the ECS does not see is not sent (gathering is skipped).
    hub.server
        .world
        .get_mut::<Health>(start)
        .expect("hp")
        .bypass_change_detection()
        .hp = 5;
    hub.run(3);
    assert_eq!(get::<Health>(&hub, a, start), Some(Health { hp: 1 }));
    // A resend delivers once and stays asleep.
    assert_eq!(
        net_resend(&mut hub.server.world, start),
        vec![hub.clients[a].peer]
    );
    hub.run(3);
    assert_eq!(get::<Health>(&hub, a, start), Some(Health { hp: 5 }));
    assert!(asleep(&hub, start));
    // A real write wakes it and is delivered (no lost edits).
    hub.server.world.get_mut::<Health>(start).expect("hp").hp = 6;
    hub.step();
    assert!(!asleep(&hub, start), "woken by the write");
    hub.run(3);
    assert_eq!(get::<Health>(&hub, a, start), Some(Health { hp: 6 }));
    // A late joiner receives an asleep entity's current state.
    hub.run(3);
    assert!(asleep(&hub, quiet));
    hub.server
        .world
        .get_mut::<Health>(quiet)
        .expect("hp")
        .bypass_change_detection()
        .hp = 42;
    let late = hub.add_client();
    hub.run(3);
    assert_eq!(get::<Health>(&hub, late, quiet), Some(Health { hp: 42 }));
    // Forced sleep through the marker, woken by removing it.
    hub.server.world.entity_mut(never).insert(NetAsleep);
    hub.run(3);
    assert!(asleep(&hub, never));
    hub.server.world.entity_mut(never).remove::<NetAsleep>();
    hub.run(1);
    assert!(!asleep(&hub, never));

    // The global switch disables sleep everywhere.
    let mut off = self::hub(11);
    off.add_client();
    off.server
        .world
        .resource_mut::<NetSettings>()
        .replication
        .sleep_enabled = false;
    let e = off
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::StartAsleep,
                ..rep()
            },
            Health { hp: 1 },
        ))
        .id();
    off.run(10);
    assert!(!asleep(&off, e));
}

#[derive(Resource, Default)]
struct VecEvents(Vec<(NetVecEventKind, u32)>);

fn collect_vec_events(world: &mut World) {
    let ev = regia_net::replication::netvec_events::<Item>(world);
    world
        .resource_mut::<VecEvents>()
        .0
        .extend(ev.into_iter().map(|e| (e.kind, e.id)));
}

/// § 14 test 11.
#[test]
fn netvec_events_and_stable_ids() {
    let mut hub = hub(12);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<VecEvents>();
    hub.clients[a].gameplay.add_systems(collect_vec_events);
    let mut list = NetVec::<Item>::new();
    let i1 = list.push(Item { count: 1 }).expect("room");
    let i2 = list.push(Item { count: 2 }).expect("room");
    let e = hub.server.world.spawn((rep(), list)).id();
    hub.run(3);
    {
        let mut v = hub.server.world.get_mut::<NetVec<Item>>(e).expect("list");
        let i3 = v.push(Item { count: 3 }).expect("room");
        v.get_mut(i1).expect("i1").count = 10;
        v.remove(i2);
        v.move_to(i3, 0);
    }
    hub.run(3);
    let ev = &hub.clients[a].world.resource::<VecEvents>().0;
    assert_eq!(
        ev,
        &vec![
            (NetVecEventKind::Added, i1),
            (NetVecEventKind::Added, i2),
            (NetVecEventKind::Removed, i2),
            (NetVecEventKind::Added, 3),
            (NetVecEventKind::Changed, i1),
        ]
    );
    let client_list = get::<NetVec<Item>>(&hub, a, e).expect("list");
    let server_list = hub.server.world.get::<NetVec<Item>>(e).expect("list");
    assert_eq!(
        client_list.ids(),
        server_list.ids(),
        "ids and order follow the server"
    );
    assert_eq!(client_list.ids(), vec![3, i1]);
    let mut full = NetVec::<Item>::new();
    for i in 0..4096 {
        full.push(Item { count: i }).expect("room");
    }
    assert!(full.push(Item { count: 0 }).is_err(), "the 4,097th add errors");
}

#[derive(Resource, Default)]
struct Teleports(u32);

fn count_teleports(ev: Res<NetEvents>, mut t: ResMut<Teleports>) {
    t.0 += ev.teleported.len() as u32;
}

/// § 14 test 12: a teleport bit survives 50 % loss and is applied once.
#[test]
fn teleport_bit_survives_loss() {
    let mut hub = hub(13);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<Teleports>();
    hub.clients[a].gameplay.add_systems(count_teleports);
    hub.down = LinkConditions {
        loss: 0.5,
        ..LinkConditions::default()
    };
    let e = hub
        .server
        .world
        .spawn((Replicated::default(), Transform::from_translation(Vec3::ZERO)))
        .id();
    hub.run(5);
    hub.server.world.get_mut::<Transform>(e).expect("t").translation = Vec3::new(500.0, 0.0, 0.0);
    assert!(net_teleport(&mut hub.server.world, e));
    for _ in 0..10 {
        hub.server.world.get_mut::<Transform>(e).expect("t").translation.x += 1.0;
        hub.step();
    }
    hub.down = LinkConditions::default();
    hub.run(20);
    assert_eq!(hub.clients[a].world.resource::<Teleports>().0, 1);
    let c = copy(&hub, a, e).expect("copy");
    let w = &hub.clients[a].world;
    assert!(w.get::<NetTeleported>(c).is_some());
    assert_eq!(w.get::<Transform>(c).map(|t| t.translation.x), Some(510.0));
}

mod lo {
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    #[derive(Clone, Debug, Component, Replicate)]
    pub struct Aim {
        #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))]
        pub x: f32,
    }
}
mod lo_again {
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    #[derive(Clone, Debug, Component, Replicate)]
    pub struct Aim {
        #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))]
        pub x: f32,
    }
}
mod hi {
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    #[derive(Clone, Debug, Component, Replicate)]
    pub struct Aim {
        #[replicate(quantize(min = 0.0, max = 1.0, bits = 9))]
        pub x: f32,
    }
}

/// § 14 test 13.
#[test]
fn protocol_hash_covers_order_and_quantization() {
    let mut w1 = World::new();
    register_replicated::<Stats>(&mut w1, ReplicateOptions::default());
    register_replicated::<Health>(&mut w1, ReplicateOptions::default());
    let mut w2 = World::new();
    register_replicated::<Health>(&mut w2, ReplicateOptions::default());
    register_replicated::<Stats>(&mut w2, ReplicateOptions::default());
    let mut w3 = World::new();
    register_replicated::<Stats>(&mut w3, ReplicateOptions::default());
    register_replicated::<Health>(&mut w3, ReplicateOptions::default());
    assert_ne!(protocol_hash(&w1), protocol_hash(&w2), "registration order");
    assert_eq!(protocol_hash(&w1), protocol_hash(&w3));
    let mut w4 = World::new();
    register_replicated::<Stats>(&mut w4, opts(Audience::OwnerOnly));
    register_replicated::<Health>(&mut w4, ReplicateOptions::default());
    assert_ne!(protocol_hash(&w1), protocol_hash(&w4), "audience variant");
    assert_eq!(layout_hash::<lo::Aim>(), layout_hash::<lo_again::Aim>());
    assert_ne!(layout_hash::<lo::Aim>(), layout_hash::<hi::Aim>(), "quantization");
}

// ---- regressions from the independent review (ADR-1643) -----------------------------------------

/// Resync requests only touch copies the client holds, are capped per tick, and a copy resynced
/// in the tick its entity is despawned is still despawned (no ghost copy).
#[test]
fn resync_is_bounded_and_never_leaves_a_ghost() {
    let mut hub = hub(20);
    let a = hub.add_client();
    let peer = hub.clients[a].peer;
    let e = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    hub.run(3);
    let id = id_of(&hub, e);
    {
        let mut rep = hub.server.world.resource_mut::<ServerReplication>();
        rep.request_resync(peer, (1000..5000).map(NetId));
        rep.request_resync(peer, [id]);
    }
    assert!(hub.server.world.despawn(e));
    hub.run(3);
    assert!(
        hub.clients[a].world.resource::<NetEntities>().is_empty(),
        "the despawn still reached the copy"
    );
}

/// A child whose parent becomes replicated after it arrives at a late client with its parent.
#[test]
fn parent_replicated_later_still_arrives_first() {
    let mut hub = hub(21);
    let parent = hub.server.world.spawn(Health { hp: 1 }).id();
    let child = hub
        .server
        .world
        .spawn((rep(), Health { hp: 2 }, ChildOf(parent)))
        .id();
    hub.run(2);
    hub.server.world.entity_mut(parent).insert(rep());
    hub.run(2);
    let a = hub.add_client();
    hub.run(3);
    let pc = copy(&hub, a, parent).expect("parent copy");
    let cc = copy(&hub, a, child).expect("child copy");
    assert_eq!(hub.clients[a].world.get::<ChildOf>(cc).map(|c| c.0), Some(pc));
}

/// An ownership change on an asleep entity reaches the new owner (owner-only values and the
/// ownership bit) without a component write.
#[test]
fn ownership_change_while_asleep_is_delivered() {
    let mut hub = hub(22);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::StartAsleep,
                ..rep()
            },
            OwnerSecret(7),
        ))
        .id();
    hub.run(5);
    assert!(asleep(&hub, e));
    assert_eq!(get::<OwnerSecret>(&hub, a, e), None);
    hub.server.world.entity_mut(e).insert(NetOwner(pa));
    hub.run(3);
    let c = copy(&hub, a, e).expect("copy");
    assert!(hub.clients[a].world.get::<NetOwnedLocally>(c).is_some());
    assert_eq!(get::<OwnerSecret>(&hub, a, e), Some(OwnerSecret(7)));
}

/// Owners follow a link to a non-replicated controller whose owner changes.
#[test]
fn owner_through_a_non_replicated_link() {
    let mut hub = hub(23);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    let controller = hub.server.world.spawn(NetOwner(pa)).id();
    let pawn = hub
        .server
        .world
        .spawn((rep(), Health { hp: 1 }, NetOwnerLink(controller)))
        .id();
    hub.run(3);
    let owned = |hub: &LoopbackHub, i: usize| {
        let c = copy(hub, i, pawn).expect("copy");
        hub.clients[i].world.get::<NetOwnedLocally>(c).is_some()
    };
    assert!(owned(&hub, a) && !owned(&hub, b));
    hub.server.world.entity_mut(controller).insert(NetOwner(pb));
    hub.run(3);
    assert!(!owned(&hub, a) && owned(&hub, b));
}

/// A copy that comes back in scope loses components the server removed meanwhile.
#[test]
fn respawned_copy_drops_removed_components() {
    let mut hub = hub(24);
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(not_hidden),
    });
    hub.server.world.init_resource::<Hidden>();
    let s1 = level_entity(&mut hub.server.world, 300, 1);
    hub.server.world.entity_mut(s1).insert(Health { hp: 5 });
    let a = hub.add_client();
    let c1 = level_entity(&mut hub.clients[a].world, 300, 0);
    hub.run(3);
    assert_eq!(hub.clients[a].world.get::<Health>(c1), Some(&Health { hp: 5 }));
    hub.server.world.resource_mut::<Hidden>().0.push(s1);
    hub.run(3);
    hub.server.world.entity_mut(s1).remove::<Health>();
    hub.run(2);
    hub.server.world.resource_mut::<Hidden>().0.clear();
    hub.run(3);
    assert!(
        hub.clients[a].world.get::<Health>(c1).is_none(),
        "full current state on return"
    );
}

/// Despawning a parent's copy keeps its replicated children; they reattach when the parent is
/// back.
#[test]
fn replicated_children_survive_the_parent_copy() {
    let mut hub = hub(25);
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(not_hidden),
    });
    hub.server.world.init_resource::<Hidden>();
    let a = hub.add_client();
    let parent = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    let child = hub
        .server
        .world
        .spawn((rep(), Health { hp: 2 }, ChildOf(parent)))
        .id();
    hub.run(3);
    let cc = copy(&hub, a, child).expect("child copy");
    hub.server.world.resource_mut::<Hidden>().0.push(parent);
    hub.run(3);
    assert!(copy(&hub, a, parent).is_none());
    assert!(
        hub.clients[a].world.get_entity(cc).is_ok(),
        "the child copy is still there"
    );
    assert_eq!(copy(&hub, a, child), Some(cc));
    hub.server.world.resource_mut::<Hidden>().0.clear();
    hub.run(3);
    let pc = copy(&hub, a, parent).expect("parent back");
    assert_eq!(hub.clients[a].world.get::<ChildOf>(cc).map(|c| c.0), Some(pc));
}

mod many {
    //! 70 registered component types: the presence bitset spans two words.
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    macro_rules! many {
        ($($n:ident),*) => {
            $(#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)] pub struct $n(pub u8);)*
            pub fn register(world: &mut World) {
                $(regia_net::replication::register_replicated::<$n>(world, Default::default());)*
            }
        };
    }
    many!(
        M00, M01, M02, M03, M04, M05, M06, M07, M08, M09, M10, M11, M12, M13, M14, M15, M16, M17, M18, M19,
        M20, M21, M22, M23, M24, M25, M26, M27, M28, M29, M30, M31, M32, M33, M34, M35, M36, M37, M38, M39,
        M40, M41, M42, M43, M44, M45, M46, M47, M48, M49, M50, M51, M52, M53, M54, M55, M56, M57, M58, M59,
        M60, M61, M62, M63, M64, M65, M66, M67, M68, M69
    );
}

/// With more than 64 registered components, removing the 70th is still sent and sleep still works.
#[test]
fn presence_bitset_beyond_64_components() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 26, many::register);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::AfterQuiet { quiet_ticks: 2 },
                ..rep()
            },
            many::M00(1),
            many::M69(2),
        ))
        .id();
    hub.run(8);
    assert!(asleep(&hub, e), "idle entities still fall asleep");
    assert_eq!(get::<many::M69>(&hub, a, e), Some(many::M69(2)));
    hub.server.world.entity_mut(e).remove::<many::M69>();
    hub.run(3);
    assert_eq!(
        get::<many::M69>(&hub, a, e),
        None,
        "removal of component 70 reached the copy"
    );
    assert_eq!(get::<many::M00>(&hub, a, e), Some(many::M00(1)));
}

/// A component inserted later reaches the copy; removing `Replicated` despawns the copies but keeps
/// the server entity (local, without its `NetId`).
#[test]
fn late_components_and_unreplicating() {
    let mut hub = hub(27);
    let a = hub.add_client();
    let e = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    hub.run(3);
    hub.server.world.entity_mut(e).insert(Pulse { n: 9 });
    hub.run(3);
    assert_eq!(get::<Pulse>(&hub, a, e), Some(Pulse { n: 9 }));
    let c = copy(&hub, a, e).expect("copy");
    hub.server.world.entity_mut(e).remove::<Replicated>();
    hub.run(3);
    assert!(hub.clients[a].world.get_entity(c).is_err(), "copy despawned");
    assert!(hub.server.world.get_entity(e).is_ok(), "server entity stays");
    assert!(hub.server.world.get::<NetId>(e).is_none());
}

/// `PositionOnly` sends only the position; `Full` adds scale.
#[test]
fn transform_modes() {
    let mut hub = hub(28);
    let a = hub.add_client();
    let rot = glam::Quat::from_rotation_y(1.0);
    let t = Transform {
        translation: Vec3::new(1.0, 2.0, 3.0),
        rotation: rot,
        scale: Vec3::splat(2.0),
    };
    let pos = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            t,
        ))
        .id();
    let full = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::Full,
                ..Replicated::default()
            },
            t,
        ))
        .id();
    hub.run(3);
    let tp = get::<Transform>(&hub, a, pos).expect("position-only copy");
    assert_eq!(tp.translation, t.translation);
    assert_eq!(tp.rotation, glam::Quat::IDENTITY);
    assert_eq!(tp.scale, Vec3::ONE);
    let tf = get::<Transform>(&hub, a, full).expect("full copy");
    assert_eq!(tf.translation, t.translation);
    assert!(tf.rotation.dot(rot).abs() > 0.999);
    assert_eq!(tf.scale, Vec3::splat(2.0));
}

/// A copy the client's own gameplay despawns is recreated from the next full state.
#[test]
fn client_despawned_copy_is_recreated() {
    let mut hub = hub(29);
    let a = hub.add_client();
    let e = hub.server.world.spawn((rep(), Health { hp: 3 })).id();
    hub.run(3);
    let c = copy(&hub, a, e).expect("copy");
    assert!(hub.clients[a].world.despawn(c));
    hub.run(5);
    assert_eq!(get::<Health>(&hub, a, e), Some(Health { hp: 3 }));
    let counters = hub.clients[a]
        .world
        .resource::<regia_net::replication::ClientReplication>()
        .counters;
    assert_eq!(counters.client_despawned_proxy, 1);
}

// ---- spec 71 § 4.5 / spec 78 § 14: binds kept until a streamed cell spawns ---------------------

fn client_rep(hub: &LoopbackHub, client: usize) -> &regia_net::replication::ClientReplication {
    hub.clients[client]
        .world
        .resource::<regia_net::replication::ClientReplication>()
}

/// The client streams level 7 and lists `guids` as replicated entities of its cells.
fn stream_guids(world: &mut World, guids: &[u128]) {
    use regia_scene::stream_grid::CellCoord;
    let mut table = regia_scene::streaming::StreamedReplicated::default();
    table.insert_level(
        regia_scene::level::LevelId(Guid::from_u128(7)),
        guids.iter().map(|g| (*g, CellCoord::new(0, 0))),
    );
    world.insert_resource(table);
}

#[test]
fn a_bind_for_an_unspawned_streamed_cell_waits_and_applies_the_latest_state() {
    let mut hub = hub(41);
    let s = level_entity(&mut hub.server.world, 200, 1);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[200]);
    hub.run(3);
    assert_eq!(
        copy(&hub, a, s),
        None,
        "no bare copy for an entity whose cell is not here"
    );
    let c = client_rep(&hub, a);
    assert_eq!(c.counters.level_entity_missing, 0, "not a content mismatch");
    assert_eq!(c.counters.level_entity_buffered, 1);
    let kept: Vec<_> = c.buffered_level_binds().collect();
    assert_eq!(kept, vec![(7, 200, id_of(&hub, s))]);
    // The server's state moves on while the bind waits: updates are skipped without a request.
    hub.server.world.get_mut::<Stats>(s).expect("stats").a = 7;
    hub.run(4);
    assert_eq!(copy(&hub, a, s), None);
    assert_eq!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .resync_loops,
        0
    );
    // The cell loads: the client's own entity binds and gets the latest state.
    let c1 = level_entity(&mut hub.clients[a].world, 200, 0);
    hub.run(4);
    assert_eq!(copy(&hub, a, s), Some(c1), "bound to the cell's entity");
    let w = &hub.clients[a].world;
    assert_eq!(
        w.get::<Stats>(c1).map(|s| s.a),
        Some(7),
        "the latest state, not the cooked one"
    );
    assert!(w.get::<NetUnbound>(c1).is_none());
    let c = client_rep(&hub, a);
    assert_eq!(c.counters.level_entity_buffer_applied, 1);
    assert_eq!(c.buffered_level_binds().count(), 0);
    assert_eq!(c.counters.level_entity_missing, 0);
}

#[test]
fn a_guid_in_no_table_is_still_a_content_mismatch() {
    let mut hub = hub(42);
    let s = level_entity(&mut hub.server.world, 210, 1);
    let a = hub.add_client();
    // The client streams the level, but this guid is in no cell.
    stream_guids(&mut hub.clients[a].world, &[211]);
    hub.run(3);
    let bare = copy(&hub, a, s).expect("a missing guid spawns bare");
    assert!(hub.clients[a].world.get::<LevelMember>(bare).is_none());
    let c = client_rep(&hub, a);
    assert_eq!(c.counters.level_entity_missing, 1);
    assert_eq!(c.counters.level_entity_buffered, 0);
}

#[test]
fn kept_binds_are_capped_and_drops_are_counted() {
    let mut hub = hub(43);
    let s1 = level_entity(&mut hub.server.world, 220, 1);
    let s2 = level_entity(&mut hub.server.world, 221, 2);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[220, 221]);
    hub.clients[a]
        .world
        .resource_mut::<regia_net::replication::ClientReplication>()
        .level_bind_cap = Some(1);
    hub.run(3);
    let c = client_rep(&hub, a);
    assert_eq!(c.buffered_level_binds().count(), 1, "at most the cap");
    assert_eq!(c.counters.level_entity_buffered, 1);
    assert!(c.counters.level_entity_buffer_dropped >= 1);
    assert_eq!(c.counters.level_entity_missing, 0);
    // Neither became a bare copy.
    assert_eq!(copy(&hub, a, s1), None);
    assert_eq!(copy(&hub, a, s2), None);
}

#[test]
fn a_despawn_while_kept_ends_the_bind_and_the_cell_entity_goes() {
    let mut hub = hub(44);
    let s = level_entity(&mut hub.server.world, 230, 1);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[230]);
    hub.run(3);
    assert_eq!(client_rep(&hub, a).buffered_level_binds().count(), 1);
    assert!(hub.server.world.despawn(s));
    hub.run(3);
    let c = client_rep(&hub, a);
    assert_eq!(c.buffered_level_binds().count(), 0);
    assert_eq!(c.counters.level_entity_buffer_cleared, 1);
    // The cell spawns its cooked copy later: the server removed it, so it goes again.
    let c1 = level_entity(&mut hub.clients[a].world, 230, 0);
    hub.run(2);
    assert!(
        hub.clients[a].world.get_entity(c1).is_err(),
        "removed on the server"
    );
    assert_eq!(client_rep(&hub, a).counters.level_entity_buffer_applied, 0);
}

#[test]
fn a_level_entity_removed_before_join_goes_when_its_cell_spawns() {
    let mut hub = hub(45);
    let s = level_entity(&mut hub.server.world, 240, 1);
    hub.run(2);
    assert!(hub.server.world.despawn(s));
    hub.run(2);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[240]);
    hub.run(3);
    let c1 = level_entity(&mut hub.clients[a].world, 240, 0);
    hub.run(2);
    assert!(hub.clients[a].world.get_entity(c1).is_err());
}

#[test]
fn a_cell_unload_keeps_the_bind_for_the_next_load_without_a_warning() {
    let mut hub = hub(46);
    let s = level_entity(&mut hub.server.world, 250, 1);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[250]);
    let c1 = level_entity(&mut hub.clients[a].world, 250, 0);
    hub.run(4);
    assert_eq!(copy(&hub, a, s), Some(c1), "bound while the cell is here");
    // The cell unloads: streaming despawns its members.
    assert!(hub.clients[a].world.despawn(c1));
    hub.run(3);
    let c = client_rep(&hub, a);
    assert_eq!(
        c.counters.client_despawned_proxy, 0,
        "a cell unload is not a lost copy"
    );
    assert_eq!(c.buffered_level_binds().count(), 1);
    assert_eq!(copy(&hub, a, s), None, "no bare copy meanwhile");
    hub.server.world.get_mut::<Stats>(s).expect("stats").a = 5;
    hub.run(3);
    // The cell loads again.
    let c2 = level_entity(&mut hub.clients[a].world, 250, 0);
    hub.run(4);
    assert_eq!(copy(&hub, a, s), Some(c2));
    assert_eq!(hub.clients[a].world.get::<Stats>(c2).map(|s| s.a), Some(5));
    assert_eq!(client_rep(&hub, a).counters.level_entity_buffer_applied, 1);
}

#[test]
fn a_closed_level_ends_its_kept_binds() {
    let mut hub = hub(47);
    level_entity(&mut hub.server.world, 260, 1);
    let a = hub.add_client();
    stream_guids(&mut hub.clients[a].world, &[260]);
    hub.run(3);
    assert_eq!(client_rep(&hub, a).buffered_level_binds().count(), 1);
    hub.clients[a]
        .world
        .resource_mut::<regia_scene::streaming::StreamedReplicated>()
        .remove_level(regia_scene::level::LevelId(Guid::from_u128(7)));
    hub.run(2);
    let c = client_rep(&hub, a);
    assert_eq!(c.buffered_level_binds().count(), 0);
    assert_eq!(c.counters.level_entity_buffer_cleared, 1);
}

/// ADR-3283 (coverage): `net_wake` wakes an asleep entity at once and refuses an entity that does
/// not replicate; `pending_acks` is zero once every connection acknowledged the latest state.
#[test]
fn net_wake_and_pending_acks() {
    let mut hub = hub(12);
    hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::StartAsleep,
                ..rep()
            },
            Health { hp: 1 },
        ))
        .id();
    hub.run(4);
    assert!(asleep(&hub, e));
    assert!(regia_net::replication::net_wake(&mut hub.server.world, e));
    assert!(!asleep(&hub, e), "awake before the next tick");
    let stranger = hub.server.world.spawn_empty().id();
    assert!(!regia_net::replication::net_wake(&mut hub.server.world, stranger));
    hub.run(4);
    let id = id_of(&hub, e);
    assert_eq!(
        hub.server.world.resource::<ServerReplication>().pending_acks(id),
        0,
        "the one client acknowledged it"
    );
}
