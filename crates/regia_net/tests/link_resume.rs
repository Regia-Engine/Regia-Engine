// SPDX-License-Identifier: MPL-2.0
//! Resume for replication games over `NetLink` (spec 120 § 8.5 and § 10.3, ADR-8298, plan N216).
//!
//! The long-outage profile of spec 120 § 15.1 (two-way 100 % loss) on one client of three apps on
//! the in-memory network, with manual clocks (no wall clock, no sleeps): a server, the phone that
//! loses signal (A) and an observer (B).
//!
//! - 11 s: shorter than the 20 s timeout, but the phone comes back from a new address (its radio
//!   dropped the socket), so the server still holds the old connection: the resume takes it over.
//! - 45 s: longer than the timeout: the server holds A dormant and A comes back within the 60 s
//!   grace.
//!
//! In both A keeps its `PeerId` and every entity it owns, B sees `NetInterrupted` on A's pawns
//! during the outage and none after, and the first frame after the resume is the full state.
//! Without a resume the grace ends: the game's `on_grace_expired` hook runs exactly once.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use regia_app::App;
use regia_ecs::prelude::{Component, bevy_ecs};
use regia_ecs::prelude::{Entity, World};
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::replication::ServerReplication;
use regia_net::replication::register_replicated;
use regia_net::resume::{GraceVerdict, NetGraceHook, NetResumeState};
use regia_net::runtime::NetNode;
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::{
    NetGame, NetId, NetInterrupted, NetMode, NetOwner, NetPlugin, PeerId, Replicate, ReplicateOptions,
    Replicated, TransformReplication,
};

/// A replicated value the server never changes after the spawn: a delta frame never carries it
/// again, only a full state does.
#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Hp(u32);

#[derive(Debug)]
struct ResumeGame;

impl NetGame for ResumeGame {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.link_resume";
    const PROTOCOL_VERSION: u32 = 1;
}

/// A socket whose path can be cut both ways (the long-outage profile's 100 % two-way loss).
struct GateSocket {
    socket: LoopbackSocket,
    down: Arc<AtomicBool>,
}

impl DatagramSocket for GateSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.down.load(Ordering::Relaxed) {
                return Ok(got);
            }
            // Lost on the way in.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// One app update of 60 Hz.
const STEP_US: u64 = 16_667;
const FRAMES_PER_S: u64 = 60;
/// Pawns the game spawns per joining peer.
const PAWNS: usize = 3;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn options() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: STEP_US,
        },
        ..LinkOptions::default()
    }
}

fn app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    // B must see the marker on A's pawns: replicating it is opt-in on the v0.6 line, on every
    // process at the same point (ADR-8387).
    NetPlugin::replicate_interrupted_marker(&mut app);
    register_replicated::<Hp>(app.world_mut(), ReplicateOptions::default());
    app
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

/// The game's server side, as the net-game template does it: a join spawns the peer's pawns, a
/// leave despawns them. Returns the link events it saw.
fn game_server_step(server: &mut App) -> Vec<LinkEvent> {
    server.update();
    let events = server.world_mut().resource_mut::<NetLink>().take_events();
    for e in &events {
        match e {
            LinkEvent::Joined { peer, .. } => {
                for _ in 0..PAWNS {
                    server.world_mut().spawn((rep(), NetOwner(*peer), Hp(100)));
                }
            }
            LinkEvent::Left { peer, .. } => {
                for e in owned(server.world_mut(), *peer) {
                    server.world_mut().despawn(e);
                }
            }
            _ => {}
        }
    }
    events
}

fn owned(world: &mut World, peer: PeerId) -> Vec<Entity> {
    let mut q = world.query::<(Entity, &NetOwner)>();
    q.iter(world)
        .filter(|(_, o)| o.0 == peer)
        .map(|(e, _)| e)
        .collect()
}

fn net_ids(world: &mut World, entities: &[Entity]) -> Vec<NetId> {
    entities
        .iter()
        .filter_map(|e| world.get::<NetId>(*e).copied())
        .collect()
}

/// The copies on a client of these server ids that carry `NetInterrupted`.
fn interrupted_copies(world: &mut World, ids: &[NetId]) -> usize {
    let mut q = world.query::<(&NetId, Option<&NetInterrupted>)>();
    q.iter(world)
        .filter(|(id, i)| ids.contains(id) && i.is_some())
        .count()
}

fn copies(world: &mut World, ids: &[NetId]) -> usize {
    let mut q = world.query::<&NetId>();
    q.iter(world).filter(|id| ids.contains(id)).count()
}

struct Rig {
    net: LoopbackNet,
    server: App,
    a: App,
    b: App,
    down: Arc<AtomicBool>,
    events: Vec<LinkEvent>,
}

impl Rig {
    fn new(port: u16) -> Self {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<ResumeGame>(Box::new(net.bind(addr(port)).expect("bind")), &options())
            .expect("host");
        let down = Arc::new(AtomicBool::new(false));
        let mut a = app(NetMode::Client);
        a.world_mut()
            .resource_mut::<NetLink>()
            .connect::<ResumeGame>(
                Box::new(GateSocket {
                    socket: net.bind(addr(0)).expect("bind"),
                    down: down.clone(),
                }),
                addr(port),
                &options(),
            )
            .expect("connect a");
        let mut b = app(NetMode::Client);
        b.world_mut()
            .resource_mut::<NetLink>()
            .connect::<ResumeGame>(Box::new(net.bind(addr(0)).expect("bind")), addr(port), &options())
            .expect("connect b");
        let mut rig = Self {
            net,
            server,
            a,
            b,
            down,
            events: Vec::new(),
        };
        rig.run(3 * FRAMES_PER_S);
        assert!(rig.a.world().resource::<NetLink>().is_connected());
        assert!(rig.b.world().resource::<NetLink>().is_connected());
        rig
    }

    fn step(&mut self) {
        let events = game_server_step(&mut self.server);
        self.events.extend(events);
        self.a.update();
        self.b.update();
    }

    fn run(&mut self, frames: u64) {
        for _ in 0..frames {
            self.step();
        }
    }

    fn a_peer(&self) -> PeerId {
        self.a
            .world()
            .resource::<regia_net::LocalPeer>()
            .0
            .expect("a has a peer id")
    }

    /// A comes back from a new address (a new socket).
    fn resume_a_from_new_address(&mut self) {
        let socket = self.net.bind(addr(0)).expect("bind");
        let resumed = self
            .a
            .world_mut()
            .resource_mut::<NetLink>()
            .resume_on(Box::new(socket))
            .expect("resume");
        assert!(resumed, "a was a client");
    }
}

/// Runs a two-way outage of `outage_s` on A, then A resumes from a new address; checks the
/// acceptance of N216.
fn outage_then_resume(port: u16, outage_s: u64) -> Rig {
    let mut rig = Rig::new(port);
    let peer = rig.a_peer();
    let pawns = owned(rig.server.world_mut(), peer);
    assert_eq!(pawns.len(), PAWNS);
    let ids = net_ids(rig.server.world_mut(), &pawns);
    assert_eq!(ids.len(), PAWNS, "the pawns replicate");
    assert_eq!(copies(rig.b.world_mut(), &ids), PAWNS, "b sees a's pawns");
    assert_eq!(interrupted_copies(rig.b.world_mut(), &ids), 0);
    assert!(
        rig.a.world().resource::<NetLink>().resume_ticket().is_some(),
        "a holds a ticket"
    );

    rig.down.store(true, Ordering::Relaxed);
    // Halfway through the outage the third client sees the pawns frozen.
    rig.run(outage_s * FRAMES_PER_S / 2);
    assert_eq!(
        interrupted_copies(rig.b.world_mut(), &ids),
        PAWNS,
        "b sees NetInterrupted on every one of a's pawns during the outage"
    );
    rig.run(outage_s * FRAMES_PER_S - outage_s * FRAMES_PER_S / 2);
    assert_eq!(
        interrupted_copies(rig.b.world_mut(), &ids),
        PAWNS,
        "still frozen at the outage's end"
    );
    // A's copies drift locally while it is away: only a full state puts the server's value back.
    {
        let world = rig.a.world_mut();
        let mut q = world.query::<(&NetId, &mut Hp)>();
        let mut drifted = 0;
        for (id, mut hp) in q.iter_mut(world) {
            if ids.contains(id) {
                hp.0 = 7;
                drifted += 1;
            }
        }
        assert_eq!(drifted, PAWNS, "a holds a copy of each pawn with its Hp");
    }
    rig.resume_a_from_new_address();
    rig.run(3 * FRAMES_PER_S);

    // The same peer, with every entity it owned.
    assert!(rig.a.world().resource::<NetLink>().is_connected(), "a is back");
    assert_eq!(rig.a_peer(), peer, "a keeps its PeerId");
    let after = owned(rig.server.world_mut(), peer);
    assert_eq!(after, pawns, "every owned entity is still there, still a's");
    let peers = rig.server.world().resource::<NetNode>().peers();
    assert_eq!(peers.len(), 2, "a (same id) and b; no third id for a: {peers:?}");
    assert!(peers.contains(&peer));
    // No leave and no second join for a.
    assert!(
        !rig.events
            .iter()
            .any(|e| matches!(e, LinkEvent::Left { peer: p, .. } if *p == peer)),
        "a never left: {:?}",
        rig.events
    );
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Joined { .. }))
            .count(),
        2,
        "a and b joined once each"
    );
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Resumed { peer: p, .. } if *p == peer))
            .count(),
        1
    );
    // The first frame after the resume is the full state: every pawn's unchanged Hp reached a
    // again (a delta frame would not carry it).
    {
        let world = rig.a.world_mut();
        let mut q = world.query::<(&NetId, &Hp)>();
        let restored: Vec<u32> = q
            .iter(world)
            .filter(|(id, _)| ids.contains(id))
            .map(|(_, hp)| hp.0)
            .collect();
        assert_eq!(restored, vec![100; PAWNS], "full state after the resume");
    }
    for id in &ids {
        assert!(
            rig.server
                .world()
                .resource::<ServerReplication>()
                .conn_holds(peer, *id),
            "a holds {id:?} again"
        );
    }
    // A sees its pawns once each (the full state landed on its copies, no duplicates).
    assert_eq!(copies(rig.a.world_mut(), &ids), PAWNS);
    // And nobody sees them frozen any more.
    assert_eq!(
        interrupted_copies(rig.b.world_mut(), &ids),
        0,
        "b: NetInterrupted gone"
    );
    assert_eq!(
        rig.server
            .world_mut()
            .query::<&NetInterrupted>()
            .iter(rig.server.world())
            .count(),
        0
    );
    let st = rig.server.world().resource::<NetResumeState>();
    assert!(st.dormant.is_empty());
    assert_eq!(st.last_resume.map(|(p, _)| p), Some(peer));
    rig
}

#[test]
fn long_outage_11_s_from_a_new_address_keeps_the_peer_id_and_its_entities() {
    let rig = outage_then_resume(7911, 11);
    let st = rig.server.world().resource::<NetResumeState>();
    // 11 s is inside the 20 s timeout: the server still held the old connection.
    assert_eq!((st.takeovers, st.resumed, st.went_dormant), (1, 0, 0));
}

#[test]
fn long_outage_45_s_keeps_the_peer_id_and_its_entities() {
    let rig = outage_then_resume(7912, 45);
    let st = rig.server.world().resource::<NetResumeState>();
    // 45 s is past the 20 s timeout: dormant from 20 s, back within the 60 s grace.
    assert_eq!((st.went_dormant, st.resumed, st.takeovers), (1, 1, 0));
    assert_eq!(st.expired, 0);
}

static HOOK_CALLS: AtomicU32 = AtomicU32::new(0);

fn despawn_at_grace_end(world: &mut World, peer: PeerId) -> GraceVerdict {
    HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    // The hook sees the peer's entities still in the world.
    assert_eq!(owned(world, peer).len(), PAWNS);
    GraceVerdict::Despawn
}

#[test]
fn grace_expiry_calls_the_hook_once() {
    let mut rig = Rig::new(7913);
    rig.server
        .world_mut()
        .insert_resource(NetGraceHook::new(despawn_at_grace_end));
    let peer = rig.a_peer();
    let pawns = owned(rig.server.world_mut(), peer);
    assert_eq!(pawns.len(), PAWNS);
    rig.down.store(true, Ordering::Relaxed);
    // 20 s timeout, then dormant through the 60 s grace: nothing leaves before its end.
    rig.run(79 * FRAMES_PER_S);
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 0, "inside the grace");
    assert_eq!(owned(rig.server.world_mut(), peer), pawns, "kept while dormant");
    assert_eq!(rig.server.world().resource::<NetResumeState>().dormant.len(), 1);
    rig.run(3 * FRAMES_PER_S);
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 1, "the grace ended: one call");
    assert!(
        owned(rig.server.world_mut(), peer).is_empty(),
        "the hook despawned them"
    );
    for e in &pawns {
        assert!(rig.server.world().get_entity(*e).is_err());
    }
    rig.run(10 * FRAMES_PER_S);
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 1, "never twice");
    let st = rig.server.world().resource::<NetResumeState>();
    assert_eq!(
        (st.expired, st.hook_calls, st.despawned_entities),
        (1, 1, PAWNS as u64)
    );
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Dormant { peer: p } if *p == peer))
            .count(),
        1
    );
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Left { peer: p, reason } if *p == peer && reason == "timeout"))
            .count(),
        1
    );
    // The slot and the id are free again: a later join is a new peer.
    assert!(!rig.server.world().resource::<NetNode>().peers().contains(&peer));
}

static KEEP_CALLS: AtomicU32 = AtomicU32::new(0);

fn keep_at_grace_end(_: &mut World, _: PeerId) -> GraceVerdict {
    KEEP_CALLS.fetch_add(1, Ordering::Relaxed);
    GraceVerdict::Keep
}

/// `Keep`: the entities stay, owned by the server, without the marker.
#[test]
fn a_keep_verdict_hands_the_entities_to_the_server() {
    let mut rig = Rig::new(7914);
    rig.server
        .world_mut()
        .insert_resource(NetGraceHook::new(keep_at_grace_end));
    let peer = rig.a_peer();
    let pawns = owned(rig.server.world_mut(), peer);
    rig.down.store(true, Ordering::Relaxed);
    rig.run(82 * FRAMES_PER_S);
    assert_eq!(KEEP_CALLS.load(Ordering::Relaxed), 1);
    for e in &pawns {
        let w = rig.server.world();
        assert!(w.get_entity(*e).is_ok(), "kept");
        assert!(w.get::<NetOwner>(*e).is_none(), "the server owns it");
        assert!(w.get::<NetInterrupted>(*e).is_none());
    }
    assert_eq!(rig.server.world().resource::<NetResumeState>().kept, 1);
}

/// A spent ticket (replayed after its resume) matches nothing: the join is a fresh peer and the
/// resumed peer keeps its connection.
#[test]
fn a_ticket_is_single_use() {
    let mut rig = Rig::new(7915);
    let peer = rig.a_peer();
    let spent = rig
        .a
        .world()
        .resource::<NetLink>()
        .resume_ticket()
        .expect("ticket");
    rig.resume_a_from_new_address();
    rig.run(2 * FRAMES_PER_S);
    assert_eq!(rig.a_peer(), peer);
    let fresh = rig
        .a
        .world()
        .resource::<NetLink>()
        .resume_ticket()
        .expect("ticket");
    assert_ne!(fresh, spent, "rotated on every accept");
    // A third client replays the spent ticket.
    let socket = rig.net.bind(addr(0)).expect("bind");
    let mut client = regia_net::client::NetClient::<ResumeGame, LoopbackSocket>::connect(
        socket,
        addr(7915),
        regia_net::client::ClientConfig {
            resume: Some(spent),
            ..regia_net::client::ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    // A NetLink server requires the app's replicated schema in the join stamp (ADR-7985): a raw
    // client declares the same hash, so only the spent ticket can keep it out.
    client.set_content_hash(regia_net::payload::combined_content_hash(
        regia_net::runtime::protocol_hash(rig.server.world()),
        0,
    ));
    let mut joined_as = None;
    for frame in 0..(3 * FRAMES_PER_S) {
        rig.step();
        let now = (frame + 1) * 17 + 3_000;
        client.update(now);
        if let Some(p) = client.peer_id() {
            joined_as = Some(p);
            break;
        }
    }
    let joined_as = joined_as.expect("the replay joined");
    assert_ne!(joined_as, peer, "a spent ticket is a fresh join");
    rig.run(FRAMES_PER_S);
    assert!(
        rig.a.world().resource::<NetLink>().is_connected(),
        "a keeps its connection"
    );
    assert_eq!(rig.a_peer(), peer);
}

/// Wire 5 (the resume ticket in the join token and the accept) and wire 6 (the resumption ticket
/// and the one-round-trip resume, ADR-8858): a build of the same game on wire 3 is refused at the
/// handshake with `protocol_mismatch`, before any key exchange.
#[test]
fn a_wire_version_3_peer_is_refused_cleanly() {
    use regia_net::handshake::{ConnectRequest, Deny, DenyReason, PacketKind, split_datagram};
    use regia_net::server::{NetServer, ServerConfig};
    use regia_net::wire::{WireWriter, fnv1a_64};
    assert_eq!(regia_net::REGIA_NET_WIRE_VERSION, 6);
    let net = LoopbackNet::new();
    let mut server = NetServer::<ResumeGame, LoopbackSocket>::with_secret(
        net.bind(addr(7916)).expect("bind"),
        ServerConfig {
            resume_grace_ms: 60_000,
            ..ServerConfig::default()
        },
        [5; 32],
    );
    // The protocol id a wire-3 build of the same game computes (`protocol_id`).
    let mut w = WireWriter::new();
    w.u32(3);
    w.raw(ResumeGame::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(ResumeGame::PROTOCOL_VERSION);
    let old_id = fnv1a_64(w.as_slice());
    assert_ne!(old_id, regia_net::protocol_id::<ResumeGame>());
    let mut old = net.bind(addr(0)).expect("bind");
    let request = ConnectRequest {
        protocol_id: old_id,
        client_nonce: 0xB0B,
        crypto_version: 2,
        crypto_required: false,
    };
    old.send_to(&request.to_datagram(), addr(7916)).expect("send");
    let mut deny = None;
    let mut buf = [0u8; 2048];
    for step in 1..=200u64 {
        server.update(step * 10);
        if let Some((n, _)) = old.recv_from(&mut buf).expect("receive")
            && let Some((PacketKind::Deny, body)) = split_datagram(&buf[..n])
        {
            deny = Some(Deny::decode(body).expect("deny"));
            break;
        }
    }
    let deny = deny.expect("the server answered");
    assert_eq!(deny.client_nonce, 0xB0B);
    assert_eq!(deny.reason, DenyReason::ProtocolMismatch);
    assert_eq!(server.session().len(), 0, "nobody joined");
}

/// The transport alone: without `resume_grace_ms` (the default, and what the session's own
/// transport uses) a timeout is a leave at once, as before.
#[test]
fn a_server_without_a_grace_keeps_no_dormant_peers() {
    use regia_net::client::{ClientConfig, NetClient};
    use regia_net::server::{NetServer, ServerConfig, ServerEvent};
    let net = LoopbackNet::new();
    for grace in [0u64, 60_000] {
        let mut server = NetServer::<ResumeGame, LoopbackSocket>::with_secret(
            net.bind(addr(7917 + grace.min(1) as u16)).expect("bind"),
            ServerConfig {
                resume_grace_ms: grace,
                ..ServerConfig::default()
            },
            [6; 32],
        );
        let down = Arc::new(AtomicBool::new(false));
        let mut client = NetClient::<ResumeGame, GateSocket>::connect(
            GateSocket {
                socket: net.bind(addr(0)).expect("bind"),
                down: down.clone(),
            },
            addr(7917 + grace.min(1) as u16),
            ClientConfig::default(),
            &(),
        )
        .expect("connect");
        let mut now = 0;
        while client.peer_id().is_none() && now < 5_000 {
            now += 10;
            client.update(now);
            server.update(now);
        }
        let peer = client.peer_id().expect("joined");
        assert_eq!(
            client.resume_ticket().is_some(),
            grace > 0,
            "a ticket only with a grace"
        );
        down.store(true, Ordering::Relaxed);
        let mut events = Vec::new();
        // Past the 30 s start grace (this raw client sends no game traffic) and the 20 s timeout.
        for _ in 0..4_000 {
            now += 10;
            client.update(now);
            server.update(now);
            while let Some(e) = server.poll_event() {
                events.push(e);
            }
        }
        let left = events
            .iter()
            .filter(|e| matches!(e, ServerEvent::PeerLeft { peer: p, .. } if *p == peer))
            .count();
        let dormant = events
            .iter()
            .filter(|e| matches!(e, ServerEvent::PeerDormant { peer: p, .. } if *p == peer))
            .count();
        if grace == 0 {
            assert_eq!((left, dormant), (1, 0), "a leave at the timeout");
            assert_eq!(server.dormant_peers().count(), 0);
        } else {
            assert_eq!(
                (left, dormant),
                (0, 1),
                "dormant at the timeout, inside the grace"
            );
            assert_eq!(
                server.dormant_peers().map(|(p, _)| p).collect::<Vec<_>>(),
                vec![peer]
            );
            assert!(server.session().is_full() || server.session().max_peers() > 1);
            assert_eq!(server.session().reserved().collect::<Vec<_>>(), vec![peer]);
        }
    }
}

/// An entity the server despawns while A is dormant is despawned on A by its first frame after
/// the resume (a fresh connection state alone would never mention it).
#[test]
fn an_entity_despawned_while_dormant_is_gone_on_the_resumed_client() {
    let mut rig = Rig::new(7919);
    let crate_e = rig.server.world_mut().spawn((rep(), Hp(5))).id();
    rig.run(FRAMES_PER_S);
    let id = *rig.server.world().get::<NetId>(crate_e).expect("replicated");
    assert_eq!(copies(rig.a.world_mut(), &[id]), 1, "a holds the crate");
    rig.down.store(true, Ordering::Relaxed);
    rig.run(25 * FRAMES_PER_S);
    assert_eq!(
        rig.server.world().resource::<NetResumeState>().dormant.len(),
        1,
        "a is dormant"
    );
    rig.server.world_mut().despawn(crate_e);
    rig.run(5 * FRAMES_PER_S);
    assert_eq!(copies(rig.b.world_mut(), &[id]), 0, "b saw the despawn");
    assert_eq!(copies(rig.a.world_mut(), &[id]), 1, "a missed it while away");
    rig.resume_a_from_new_address();
    rig.run(3 * FRAMES_PER_S);
    assert!(rig.a.world().resource::<NetLink>().is_connected());
    assert_eq!(
        copies(rig.a.world_mut(), &[id]),
        0,
        "the resume's first frame despawned it"
    );
}
