// SPDX-License-Identifier: MPL-2.0
//! The replication runtime over the real endpoints (`NetServer` / `NetClient`) through
//! `regia_net::bridge`, on the in-memory datagram network: joins, a replicated entity larger than
//! one frame part, field deltas, a to-server message, and a node-decided kick. Time is a step
//! counter (16 ms per step); nothing sleeps.

use std::net::SocketAddr;

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, FRAME_PART_BYTES, MAX_BACKLOG_BYTES, NetFrame, ServerBridge};
use regia_net::handshake::DisconnectReason;
use regia_net::message::register_net_message;
use regia_net::replication::register_replicated;
use regia_net::runtime::{Datagram, NetEntities, NetNode, run_net_tick};
use regia_net::*;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.bridge";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp(u32);

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Blob {
    #[replicate(max_len = 1000000)]
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Ping {
    n: u32,
}
impl NetMessage for Ping {
    const DIRECTION: Direction = Direction::ToServer;
    fn validate(&self, _cx: &ValidateContext) -> Verdict {
        if self.n == 99 {
            Verdict::Kick("cheat")
        } else {
            Verdict::Accept
        }
    }
}
impl ToServerMessage for Ping {}

fn setup(world: &mut World, mode: NetMode) {
    world.insert_resource(mode);
    register_replicated::<Hp>(world, ReplicateOptions::default());
    register_replicated::<Blob>(world, ReplicateOptions::default());
    register_net_message::<Ping>(world);
    regia_net::runtime::ensure_resources(world);
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

struct Rig {
    server: NetServer<Game, LoopbackSocket>,
    client: NetClient<Game, LoopbackSocket>,
    sw: World,
    cw: World,
    sb: ServerBridge,
    cb: ClientBridge,
    gs: Schedule,
    gc: Schedule,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        let net = LoopbackNet::new();
        let server = NetServer::with_secret(
            net.bind(addr(7000)).expect("bind"),
            ServerConfig::default(),
            [3; 32],
        );
        let client = NetClient::connect(
            net.bind(addr(0)).expect("bind"),
            addr(7000),
            ClientConfig::default(),
            &(),
        )
        .expect("join fits");
        let mut sw = World::new();
        setup(&mut sw, NetMode::DedicatedServer);
        let mut cw = World::new();
        setup(&mut cw, NetMode::Client);
        Self {
            server,
            client,
            sw,
            cw,
            sb: ServerBridge::new(),
            cb: ClientBridge::new(),
            gs: Schedule::default(),
            gc: Schedule::default(),
            now: 0,
        }
    }

    fn step(&mut self) {
        self.now += 16;
        self.server.receive(self.now);
        self.sb.pump_in(&mut self.sw, &mut self.server);
        run_net_tick(&mut self.sw, &mut self.gs);
        self.sb.pump_out(&mut self.sw, &mut self.server);
        self.server.flush(self.now);

        self.client.receive(self.now);
        self.cb.pump_in(&mut self.cw, &mut self.client);
        run_net_tick(&mut self.cw, &mut self.gc);
        self.cb.pump_out(&mut self.cw, &mut self.client);
        self.client.flush(self.now);
    }

    fn run(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    fn copy(&self, e: Entity) -> Option<Entity> {
        let id = *self.sw.get::<NetId>(e)?;
        self.cw.resource::<NetEntities>().entity(id)
    }
}

#[test]
fn replication_over_the_udp_endpoints() {
    let mut rig = Rig::new();
    rig.run(10);
    assert!(rig.client.is_connected());
    let peer = rig.client.peer_id().expect("peer id");
    assert_eq!(rig.cw.resource::<LocalPeer>().0, Some(peer));
    assert_eq!(rig.sw.resource::<NetNode>().peers(), vec![peer]);

    // An entity whose spawn record needs several frame parts.
    let big: Vec<u8> = (0..(FRAME_PART_BYTES * 3 + 11))
        .map(|i| (i % 251) as u8)
        .collect();
    let e = rig
        .sw
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            NetOwner(peer),
            Hp(10),
            Blob { bytes: big.clone() },
        ))
        .id();
    rig.run(40);
    let c = rig.copy(e).expect("copy over UDP");
    assert_eq!(rig.cw.get::<Hp>(c), Some(&Hp(10)));
    assert_eq!(rig.cw.get::<Blob>(c).map(|b| b.bytes.len()), Some(big.len()));
    assert!(rig.cw.get::<NetOwnedLocally>(c).is_some());

    // A field delta.
    rig.sw.get_mut::<Hp>(e).expect("hp").0 = 11;
    rig.run(10);
    assert_eq!(rig.cw.get::<Hp>(c), Some(&Hp(11)));

    // A to-server message from the owner.
    let mut once = Some(c);
    rig.cw
        .run_system_once(move |mut net: NetSender| {
            if let Some(c) = once.take() {
                net.send_to_server(c, Ping { n: 1 }).expect("queued");
            }
        })
        .expect("runs");
    rig.run(10);
    let reg = rig.sw.resource::<regia_net::message::NetMessageRegistry>();
    assert_eq!(reg.counters[0].delivered, 1);

    // The node kicks (validation) and the endpoint disconnects the client.
    let mut once = Some(c);
    rig.cw
        .run_system_once(move |mut net: NetSender| {
            if let Some(c) = once.take() {
                net.send_to_server(c, Ping { n: 99 }).expect("queued");
            }
        })
        .expect("runs");
    rig.run(20);
    assert!(!rig.client.is_connected());
    assert_eq!(
        rig.cw.resource::<NetNode>().closed,
        Some(DisconnectReason::InvalidMessage)
    );
    assert!(rig.sw.resource::<NetNode>().peers().is_empty());
}

/// Audit H3 (muse) / M6 (muse2): a client that stops acknowledging cannot grow the server: the send
/// backlog stops at its limit and the peer is disconnected with `ReliableOverflow`.
#[test]
fn a_stalled_client_is_disconnected_at_the_backlog_limit() {
    let mut r = Rig::new();
    while !r.client.is_connected() {
        r.step();
        assert!(r.now < 2_000, "no join");
    }
    r.step();
    let peer = r.client.peer_id().expect("connected");
    assert!(r.sw.resource::<NetNode>().has_peer(peer));
    // The client's process hangs: nothing below reads its socket. The node keeps producing a
    // large frame a tick.
    let mut left = None;
    for i in 0..64 {
        r.now += 16;
        r.server.receive(r.now);
        r.sb.pump_in(&mut r.sw, &mut r.server);
        r.sw.resource_mut::<NetNode>().outbox.push(Datagram {
            peer,
            channel: regia_net::message::Channel::ReliableOrdered,
            bytes: vec![0; 1024 * 1024],
        });
        r.sb.pump_out(&mut r.sw, &mut r.server);
        assert!(r.sb.backlog_bytes(peer) <= MAX_BACKLOG_BYTES);
        r.server.flush(r.now);
        while let Some(event) = r.server.poll_event() {
            if let ServerEvent::PeerLeft { reason, .. } = event {
                left = Some((i, reason));
            }
        }
        if left.is_some() {
            break;
        }
    }
    let (ticks, reason) = left.expect("the peer was removed");
    assert_eq!(reason, DisconnectReason::ReliableOverflow);
    assert!(
        ticks >= 8,
        "the fragments in flight and the backlog held {ticks} frames first"
    );
    assert!(!r.sw.resource::<NetNode>().has_peer(peer));
}

/// Audit net3-correctness #1 (grok): when the client's send backlog passes its limit, the client
/// ends its connection. It never writes a later frame onto the reliable stream behind the head of
/// a frame whose tail was dropped, which the server would reassemble as one spliced frame.
#[test]
fn a_client_backlog_overflow_never_splices_frames() {
    const BIG: u8 = 0xAA;
    const SMALL: u8 = 0x55;
    let mut r = Rig::new();
    while !r.client.is_connected() {
        r.step();
        assert!(r.now < 2_000, "no join");
    }
    r.step();
    let peer = r.client.peer_id().expect("connected");
    // The server reads what the client's last step sent; after this only the frames below are in
    // flight.
    r.now += 16;
    r.server.receive(r.now);
    r.sb.pump_in(&mut r.sw, &mut r.server);
    r.sw.resource_mut::<NetNode>().inbox.clear();
    r.server.flush(r.now);
    // The server hangs: nothing reads its socket, so the client's window fills and its bridge
    // backlog grows by one client-sized frame a tick until it passes the limit.
    let frame_bytes = regia_net::bridge::MAX_CLIENT_FRAME_BYTES;
    let mut overflowed = false;
    for _ in 0..64 {
        r.now += 16;
        r.client.receive(r.now);
        r.cb.pump_in(&mut r.cw, &mut r.client);
        r.cw.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: regia_net::message::Channel::ReliableOrdered,
            bytes: vec![BIG; frame_bytes],
        });
        r.cb.pump_out(&mut r.cw, &mut r.client);
        r.client.flush(r.now);
        if r.cw.resource::<NetNode>().closed.is_some() {
            overflowed = true;
            break;
        }
    }
    assert!(overflowed, "the client backlog passed its limit");
    // The node keeps producing small frames after the overflow.
    for _ in 0..4 {
        r.now += 16;
        r.client.receive(r.now);
        r.cb.pump_in(&mut r.cw, &mut r.client);
        r.cw.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: regia_net::message::Channel::ReliableOrdered,
            bytes: vec![SMALL; 50],
        });
        r.cb.pump_out(&mut r.cw, &mut r.client);
        r.client.flush(r.now);
    }
    assert!(
        !r.client.is_connected(),
        "an overflowed client ends its connection"
    );
    assert_eq!(
        r.cw.resource::<NetNode>().closed,
        Some(DisconnectReason::ReliableOverflow)
    );
    // The server wakes up and reads everything the client sent. Every frame it reassembles is one
    // whole frame the node queued: never a head of big bytes followed by small bytes.
    for _ in 0..400 {
        r.now += 16;
        r.server.receive(r.now);
        r.sb.pump_in(&mut r.sw, &mut r.server);
        let inbox = std::mem::take(&mut r.sw.resource_mut::<NetNode>().inbox);
        for d in inbox.iter().filter(|d| d.peer == peer) {
            let whole_big = d.bytes.len() == frame_bytes && d.bytes.iter().all(|b| *b == BIG);
            let whole_small = d.bytes.len() == 50 && d.bytes.iter().all(|b| *b == SMALL);
            assert!(
                whole_big || whole_small,
                "a spliced frame of {} bytes ({} big, {} small) reached the server",
                d.bytes.len(),
                d.bytes.iter().filter(|b| **b == BIG).count(),
                d.bytes.iter().filter(|b| **b == SMALL).count(),
            );
            assert!(!whole_small, "a frame queued after the overflow was sent");
        }
        r.server.flush(r.now);
        r.client.receive(r.now);
        r.cb.pump_in(&mut r.cw, &mut r.client);
        r.cb.pump_out(&mut r.cw, &mut r.client);
        r.client.flush(r.now);
    }
}

/// Review: a world that was on another server joins this one, whatever tick that server counted to.
#[test]
fn a_new_connection_forgets_the_old_servers_ticks() {
    let mut r = Rig::new();
    r.cw.resource_mut::<regia_net::replication::ClientReplication>()
        .applied = Some(NetTick(50_000_000));
    while !r.client.is_connected() {
        r.step();
        assert!(r.now < 2_000, "no join");
    }
    for _ in 0..5 {
        r.step();
    }
    assert_eq!(r.cw.resource::<NetNode>().closed, None);
    let applied =
        r.cw.resource::<regia_net::replication::ClientReplication>()
            .applied
            .expect("frames applied");
    assert!(applied.0 < 1_000, "{applied:?}");
}
