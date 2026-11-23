// SPDX-License-Identifier: MPL-2.0
//! Composed spec 70 §6.5 histories. The oracle is a hand-written timeline, not an ownership
//! epoch model: ToServer checks the processing tick; ToOwner resolves at Send. All delays are
//! fixed hub steps, and every seed is replayed from a fresh three-process hub.

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::message::{Channel, MessageCounters, NetMessageRegistry, register_net_message};
use regia_net::replication::{ConnWire, ServerReplication};
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::testing::{FrameFate, LoopbackHub};
use regia_net::*;

trait Value: NetMessage {
    fn new(value: u32) -> Self;
    fn value(&self) -> u32;
}

macro_rules! message {
    ($name:ident, $direction:ident, $marker:ident, $channel:ident) => {
        #[derive(Clone, Debug, PartialEq, Replicate)]
        struct $name {
            value: u32,
        }
        impl NetMessage for $name {
            const DIRECTION: Direction = Direction::$direction;
            const CHANNEL: Channel = Channel::$channel;
        }
        impl $marker for $name {}
        impl Value for $name {
            fn new(value: u32) -> Self {
                Self { value }
            }
            fn value(&self) -> u32 {
                self.value
            }
        }
    };
}
message!(ReliableInput, ToServer, ToServerMessage, ReliableOrdered);
message!(SequencedInput, ToServer, ToServerMessage, UnreliableSequenced);
message!(ReliableNotice, ToOwner, ToOwnerMessage, ReliableOrdered);
message!(SequencedNotice, ToOwner, ToOwnerMessage, UnreliableSequenced);

#[derive(Resource, Default)]
struct Step(u64);

// NetId, rather than a local ECS slot, makes replacement identity explicit in the oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    step: u64,
    value: u32,
    target: Option<NetId>,
    from: PeerId,
    owner: Option<PeerId>,
}

#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
struct Log(Vec<Entry>);

fn record<M: Value>(
    messages: Res<NetMessages<M>>,
    step: Res<Step>,
    node: Res<NetNode>,
    targets: Query<(&NetId, Option<&NetOwner>)>,
    mut log: ResMut<Log>,
) {
    for m in messages.iter() {
        assert!(node.tick >= m.sent_tick, "state precedes the handler");
        let target = m.target.map(|e| targets.get(e).expect("live replicated target"));
        log.0.push(Entry {
            step: step.0,
            value: m.msg.value(),
            target: target.map(|(id, _)| *id),
            from: m.from,
            owner: target.and_then(|(_, owner)| owner.map(|o| o.0)),
        });
    }
}

fn setup(world: &mut World) {
    register_net_message::<ReliableInput>(world);
    register_net_message::<SequencedInput>(world);
    register_net_message::<ReliableNotice>(world);
    register_net_message::<SequencedNotice>(world);
    world.init_resource::<Step>();
    world.init_resource::<Log>();
}

fn state_setup(world: &mut World) {
    setup(world);
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
}

fn hub<I: Value, O: Value>(seed: u64, state: bool) -> LoopbackHub {
    let mut hub = LoopbackHub::new(
        NetMode::DedicatedServer,
        seed,
        if state { state_setup } else { setup },
    );
    hub.add_client();
    hub.add_client();
    for p in std::iter::once(&mut hub.server).chain(hub.clients.iter_mut()) {
        p.gameplay.add_systems((record::<I>, record::<O>).chain());
    }
    hub
}

fn step(hub: &mut LoopbackHub) {
    let next = hub.step_count() + 1;
    for p in std::iter::once(&mut hub.server).chain(hub.clients.iter_mut()) {
        p.world.resource_mut::<Step>().0 = next;
    }
    hub.step();
}

fn through(hub: &mut LoopbackHub, last: u64) {
    while hub.step_count() < last {
        step(hub);
    }
}

fn send(world: &mut World, f: impl FnOnce(&mut NetSender) + Send + Sync + 'static) {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| {
            f.take().expect("called once")(&mut net);
        })
        .expect("send system");
}

fn input<I: Value + ToServerMessage>(
    hub: &mut LoopbackHub,
    client: usize,
    target: Option<NetId>,
    value: u32,
) {
    let world = &mut hub.clients[client].world;
    let entity = target.map(|id| world.resource::<NetEntities>().entity(id).expect("client copy"));
    send(world, move |net| {
        if let Some(entity) = entity {
            net.send_to_server(entity, I::new(value)).expect("queued input");
        } else {
            net.send_session_to_server(I::new(value))
                .expect("queued session input");
        }
    });
}

fn notice<O: Value + ToOwnerMessage>(hub: &mut LoopbackHub, target: Entity, value: u32) {
    send(&mut hub.server.world, move |net| {
        net.send_to_owner(target, O::new(value)).expect("queued notice");
    });
}

fn session<O: Value + ToOwnerMessage>(hub: &mut LoopbackHub, peer: PeerId, value: u32) {
    send(&mut hub.server.world, move |net| {
        net.send_session_to_peer(peer, O::new(value))
            .expect("queued session notice");
    });
}

fn spawn(hub: &mut LoopbackHub, owner: PeerId) -> Entity {
    hub.server
        .world
        .spawn((Replicated::default(), NetOwner(owner)))
        .id()
}

fn id(hub: &LoopbackHub, entity: Entity) -> NetId {
    *hub.server.world.get::<NetId>(entity).expect("assigned id")
}

fn entry(step: u64, value: u32, target: Option<NetId>, from: PeerId, owner: Option<PeerId>) -> Entry {
    Entry {
        step,
        value,
        target,
        from,
        owner,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    logs: Vec<Log>,
    counters: Vec<(MessageCounters, MessageCounters)>,
}

fn counters<M: NetMessage>(world: &World) -> MessageCounters {
    let registry = world.resource::<NetMessageRegistry>();
    registry.counters[usize::from(registry.index_of::<M>().expect("registered"))]
}

fn counts<M: NetMessage>(world: &World, expected: (u64, u64, u64, u64, u64)) {
    let c = counters::<M>(world);
    assert_eq!(
        (c.sent, c.received, c.delivered, c.not_owner, c.unresolved),
        expected,
        "counters for {}",
        std::any::type_name::<M>()
    );
    assert_eq!((c.rejected, c.rate_limited, c.dropped_overflow), (0, 0, 0));
}

fn outcome<I: Value, O: Value>(hub: &LoopbackHub, expected: [Vec<Entry>; 3]) -> Outcome {
    let processes = std::iter::once(&hub.server).chain(hub.clients.iter());
    let mut result = Outcome {
        logs: Vec::new(),
        counters: Vec::new(),
    };
    for (p, expected) in processes.zip(expected) {
        assert!(p.connected);
        assert_eq!(p.world.resource::<NetNode>().closed, None);
        let log = p.world.resource::<Log>().clone();
        assert_eq!(log.0, expected, "handler history for {:?}", p.peer);
        result.logs.push(log);
        result
            .counters
            .push((counters::<I>(&p.world), counters::<O>(&p.world)));
        for c in [counters::<I>(&p.world), counters::<O>(&p.world)] {
            assert_eq!((c.rejected, c.rate_limited, c.dropped_overflow), (0, 0, 0));
        }
    }
    result
}

fn server_history<I: Value + ToServerMessage, O: Value + ToOwnerMessage>(
    seed: u64,
    handoff: bool,
) -> Outcome {
    let mut hub = hub::<I, O>(seed, false);
    let (a, b) = (hub.clients[0].peer, hub.clients[1].peer);
    let target = spawn(&mut hub, a);
    through(&mut hub, 3);
    let target_id = id(&hub, target);
    // A's first frame runs during B's tenure (step 6), its second after return to A (10).
    // B's delayed frame runs after return to A (11). Reliable ordering holds A's session too.
    hub.up_filter = Some(Box::new(move |d, step| match (d.peer, step) {
        (p, 4) if p == a => FrameFate::Delay(1),
        (p, 5) if p == a => FrameFate::Delay(4),
        (p, 7) if p == b => FrameFate::Delay(3),
        _ => FrameFate::Deliver,
    }));
    input::<I>(&mut hub, 0, Some(target_id), 10);
    input::<I>(&mut hub, 0, None, 100);
    step(&mut hub); // 4: first A frame in flight; A still owns the target
    // Queue under A before either handoff. This message remains delayed through A -> B -> A.
    input::<I>(&mut hub, 0, Some(target_id), 11);
    notice::<O>(&mut hub, target, 50);
    input::<I>(&mut hub, 1, Some(target_id), 12);
    input::<I>(&mut hub, 1, None, 101);
    session::<O>(&mut hub, a, 200);
    session::<O>(&mut hub, b, 201);
    if handoff {
        hub.server.world.entity_mut(target).insert(NetOwner(b));
    }
    // 5: A -> B; 6: first A frame is rejected, while B's input and both sessions run.
    through(&mut hub, 6);
    assert_eq!(counters::<I>(&hub.server.world).not_owner, 1);
    input::<I>(&mut hub, 1, Some(target_id), 13);
    step(&mut hub); // 7: B's frame in flight
    notice::<O>(&mut hub, target, 51);
    if handoff {
        hub.server.world.entity_mut(target).insert(NetOwner(a));
    }
    through(&mut hub, 11);
    input::<I>(&mut hub, 0, Some(target_id), 14);
    through(&mut hub, 14);
    let mut server = Vec::new();
    if !handoff {
        server.push(entry(6, 10, Some(target_id), a, Some(a)));
    }
    server.push(entry(6, 100, None, a, None));
    if handoff {
        server.push(entry(6, 12, Some(target_id), b, Some(b)));
    }
    server.push(entry(6, 101, None, b, None));
    server.push(entry(10, 11, Some(target_id), a, Some(a)));
    server.push(entry(13, 14, Some(target_id), a, Some(a)));
    assert_eq!(counters::<I>(&hub.server.world).not_owner, 2);
    assert_eq!(counters::<I>(&hub.server.world).unresolved, 0);
    counts::<I>(&hub.server.world, (0, 7, 5, 2, 0));
    counts::<I>(&hub.clients[0].world, (4, 0, 0, 0, 0));
    counts::<I>(&hub.clients[1].world, (3, 0, 0, 0, 0));
    counts::<O>(&hub.server.world, (4, 0, 0, 0, 0));
    counts::<O>(
        &hub.clients[0].world,
        (0, if handoff { 2 } else { 3 }, if handoff { 2 } else { 3 }, 0, 0),
    );
    counts::<O>(
        &hub.clients[1].world,
        (0, if handoff { 2 } else { 1 }, if handoff { 2 } else { 1 }, 0, 0),
    );
    let mut a_log = Vec::new();
    let mut b_log = Vec::new();
    if handoff {
        b_log.push(entry(6, 50, Some(target_id), PeerId::SERVER, None));
    } else {
        a_log.push(entry(6, 50, Some(target_id), PeerId::SERVER, None));
    }
    a_log.push(entry(6, 200, None, PeerId::SERVER, None));
    a_log.push(entry(9, 51, Some(target_id), PeerId::SERVER, None));
    b_log.push(entry(6, 201, None, PeerId::SERVER, None));
    outcome::<I, O>(&hub, [server, a_log, b_log])
}

fn owner_history<I: Value + ToServerMessage, O: Value + ToOwnerMessage>(seed: u64) -> Outcome {
    let mut hub = hub::<I, O>(seed, false);
    let (a, b) = (hub.clients[0].peer, hub.clients[1].peer);
    let target = spawn(&mut hub, a);
    through(&mut hub, 3);
    let target_id = id(&hub, target);
    let delayed_tag = if O::CHANNEL.is_reliable() { 1 } else { 2 };
    hub.down_filter = Some(Box::new(move |d, step| {
        if d.peer == b && step == 4 && d.bytes.first() == Some(&delayed_tag) {
            FrameFate::Delay(4)
        } else {
            FrameFate::Deliver
        }
    }));
    notice::<O>(&mut hub, target, 20); // queued while A owns, routed to B at Send
    hub.server.world.entity_mut(target).insert(NetOwner(b));
    step(&mut hub); // 4
    session::<O>(&mut hub, b, 200);
    through(&mut hub, 6);
    notice::<O>(&mut hub, target, 21);
    step(&mut hub); // 7: routed to B, arrives 8 (sequenced) or 9 (reliable)
    notice::<O>(&mut hub, target, 22); // queued while B owns, routed to A at Send
    hub.server.world.entity_mut(target).insert(NetOwner(a));
    through(&mut hub, 9);
    notice::<O>(&mut hub, target, 23);
    input::<I>(&mut hub, 1, None, 100);
    through(&mut hub, 13);
    let b_log = if O::CHANNEL.is_reliable() {
        vec![
            entry(9, 20, Some(target_id), PeerId::SERVER, None),
            entry(9, 200, None, PeerId::SERVER, None),
            entry(9, 21, Some(target_id), PeerId::SERVER, None),
        ]
    } else {
        // The session has a separate sequence key. 21 overtakes 20 on B; 22 on A cannot
        // suppress B's stream. The old 20 is received but suppressed, not an authority drop.
        vec![
            entry(6, 200, None, PeerId::SERVER, None),
            entry(8, 21, Some(target_id), PeerId::SERVER, None),
        ]
    };
    assert_eq!(counters::<O>(&hub.clients[1].world).received, 3);
    assert_eq!(counters::<O>(&hub.clients[1].world).unresolved, 0);
    assert_eq!(counters::<O>(&hub.clients[1].world).not_owner, 0);
    counts::<I>(&hub.server.world, (0, 1, 1, 0, 0));
    counts::<I>(&hub.clients[0].world, (0, 0, 0, 0, 0));
    counts::<I>(&hub.clients[1].world, (1, 0, 0, 0, 0));
    counts::<O>(&hub.server.world, (5, 0, 0, 0, 0));
    counts::<O>(&hub.clients[0].world, (0, 2, 2, 0, 0));
    counts::<O>(
        &hub.clients[1].world,
        (0, 3, if O::CHANNEL.is_reliable() { 3 } else { 2 }, 0, 0),
    );
    outcome::<I, O>(
        &hub,
        [
            vec![entry(11, 100, None, b, None)],
            vec![
                entry(9, 22, Some(target_id), PeerId::SERVER, None),
                entry(11, 23, Some(target_id), PeerId::SERVER, None),
            ],
            b_log,
        ],
    )
}

fn lifetime_history<I: Value + ToServerMessage, O: Value + ToOwnerMessage>(seed: u64) -> Outcome {
    let mut hub = hub::<I, O>(seed, true);
    let (a, b) = (hub.clients[0].peer, hub.clients[1].peer);
    through(&mut hub, 6);
    assert_eq!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .state_link(a)
            .expect("link")
            .wire,
        ConnWire::Acked
    );
    // Keep A's spawn unresolved while B receives it. State packets may be dropped; reliable
    // notices still travel on their ordered stream. No fabricated wire bytes or id reuse.
    hub.down_filter = Some(Box::new(move |d, step| {
        if d.peer == a && (7..=9).contains(&step) && d.bytes.first() == Some(&7) {
            FrameFate::Drop
        } else {
            FrameFate::Deliver
        }
    }));
    let victim = spawn(&mut hub, a);
    step(&mut hub); // 7: assign victim id, hold A's spawn
    let victim_id = id(&hub, victim);
    notice::<O>(&mut hub, victim, 30);
    through(&mut hub, 9);
    assert_eq!(
        hub.clients[0].world.resource::<NetEntities>().entity(victim_id),
        None
    );
    assert!(
        hub.clients[1]
            .world
            .resource::<NetEntities>()
            .entity(victim_id)
            .is_some()
    );
    assert_eq!(hub.clients[0].world.resource::<NetNode>().pending_counts().1, 1);
    assert!(hub.clients[0].world.resource::<Log>().0.is_empty());
    assert!(hub.server.world.despawn(victim));
    let replacement = spawn(&mut hub, a);
    session::<O>(&mut hub, a, 40);
    session::<O>(&mut hub, b, 42);
    input::<I>(&mut hub, 1, None, 100);
    step(&mut hub); // 10: despawn victim, assign distinct replacement id
    let replacement_id = id(&hub, replacement);
    assert_ne!(victim_id, replacement_id);
    notice::<O>(&mut hub, replacement, 41);
    through(&mut hub, 12);
    assert_eq!(
        hub.clients[0].world.resource::<NetNode>().pending_counts().1,
        1,
        "unresolved old target cannot bind to the replacement"
    );
    through(&mut hub, 45); // first seen at 9, 32-step bound expires at 41
    for p in &hub.clients {
        assert_eq!(p.world.resource::<NetEntities>().entity(victim_id), None);
        assert!(p.world.resource::<NetEntities>().entity(replacement_id).is_some());
        assert_eq!(p.world.resource::<NetNode>().pending_counts().1, 0);
    }
    let c = counters::<O>(&hub.clients[0].world);
    assert_eq!((c.received, c.delivered, c.unresolved, c.not_owner), (3, 2, 1, 0));
    assert_eq!(counters::<O>(&hub.clients[1].world).unresolved, 0);
    counts::<I>(&hub.server.world, (0, 1, 1, 0, 0));
    counts::<I>(&hub.clients[0].world, (0, 0, 0, 0, 0));
    counts::<I>(&hub.clients[1].world, (1, 0, 0, 0, 0));
    counts::<O>(&hub.server.world, (4, 0, 0, 0, 0));
    counts::<O>(&hub.clients[0].world, (0, 3, 2, 0, 1));
    counts::<O>(&hub.clients[1].world, (0, 1, 1, 0, 0));
    outcome::<I, O>(
        &hub,
        [
            vec![entry(11, 100, None, b, None)],
            vec![
                entry(11, 40, None, PeerId::SERVER, None),
                entry(12, 41, Some(replacement_id), PeerId::SERVER, None),
            ],
            vec![entry(11, 42, None, PeerId::SERVER, None)],
        ],
    )
}

macro_rules! histories {
    ($server:ident, $control:ident, $owner:ident, $lifetime:ident, $input:ty, $notice:ty) => {
        #[test]
        fn $server() {
            for seed in [11, 29, 73] {
                assert_eq!(
                    server_history::<$input, $notice>(seed, true),
                    server_history::<$input, $notice>(seed, true)
                );
            }
        }
        #[test]
        fn $control() {
            for seed in [11, 29, 73] {
                assert_eq!(
                    server_history::<$input, $notice>(seed, false),
                    server_history::<$input, $notice>(seed, false)
                );
            }
        }
        #[test]
        fn $owner() {
            for seed in [11, 29, 73] {
                assert_eq!(
                    owner_history::<$input, $notice>(seed),
                    owner_history::<$input, $notice>(seed)
                );
            }
        }
        #[test]
        fn $lifetime() {
            for seed in [11, 29, 73] {
                assert_eq!(
                    lifetime_history::<$input, $notice>(seed),
                    lifetime_history::<$input, $notice>(seed)
                );
            }
        }
    };
}
histories!(
    reliable_processing_tick_authority,
    reliable_no_handoff_control,
    reliable_owner_routing_at_send,
    reliable_unresolved_despawn,
    ReliableInput,
    ReliableNotice
);
histories!(
    sequenced_processing_tick_authority,
    sequenced_no_handoff_control,
    sequenced_owner_routing_at_send,
    sequenced_unresolved_despawn,
    SequencedInput,
    SequencedNotice
);
