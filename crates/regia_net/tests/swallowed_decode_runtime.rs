// SPDX-License-Identifier: MPL-2.0
//! Swallowed decode errors in the replication runtime (spec 120 § 11.2, ADR-7984), over real UDP
//! loopback with encryption required, through `regia_net::bridge`: a client that sends a
//! server-only message type is disconnected with a protocol error and named, and voice packets that
//! do not parse are charged to their sender (one malformed point each, after a token of its voice
//! rate bucket) until it is dropped. A valid client in the same session is never affected. Time is a
//! step counter (16 ms per step); a pump reads the sockets until every datagram the other side sent
//! has arrived, so nothing sleeps.

use std::net::SocketAddr;

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::crypto::CryptoMode;
use regia_net::handshake::DisconnectReason;
use regia_net::message::{NetMessageRegistry, register_net_message};
use regia_net::runtime::{NetNode, run_net_tick};
use regia_net::server::RejectKind;
use regia_net::wire::WireError;
use regia_net::*;

const STEP_MS: u64 = 16;
/// Socket reads one pump may spend waiting for in-flight loopback datagrams.
const PUMP_SPINS: usize = 200_000;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.swallowed_decode_runtime";
    const PROTOCOL_VERSION: u32 = 1;
}

/// A session message every client may send.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Ping {
    n: u32,
}
impl NetMessage for Ping {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for Ping {}

/// A broadcast only the server may send. Registered second on the server and the honest client.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Flash {
    n: u32,
}
impl NetMessage for Flash {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for Flash {}

/// The hostile client's stand-in for `Flash`: the same layout and the same registry index, but
/// declared to-server, so its sender queues it and the wire carries `Flash`'s type index.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Forged {
    n: u32,
}
impl NetMessage for Forged {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for Forged {}

type Server = NetServer<Game, UdpTransport>;
type Client = NetClient<Game, UdpTransport>;

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

struct Peer {
    client: Client,
    world: World,
    bridge: ClientBridge,
    gameplay: Schedule,
    peer: PeerId,
}

/// A dedicated server world and two client worlds (`a` honest, `b` the offender) on sealed UDP
/// links.
struct Rig {
    server: Server,
    world: World,
    bridge: ServerBridge,
    gameplay: Schedule,
    a: Peer,
    b: Peer,
    now: u64,
    events: Vec<ServerEvent<Game>>,
}

impl Rig {
    /// `register` sets up a world's messages: `(world, is_offender)`. `None` when the host has no
    /// loopback UDP.
    fn start(register: fn(&mut World, bool), server_gameplay: Schedule) -> Option<Self> {
        let Ok(socket) = UdpTransport::bind(localhost()) else {
            println!("SKIP no loopback UDP");
            return None;
        };
        let config = ServerConfig {
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        };
        let server: Server = NetServer::with_secret(socket, config, [5; 32]);
        let addr = server.local_addr().expect("server bound");
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        register(&mut world, false);
        regia_net::runtime::ensure_resources(&mut world);
        let peer = |name: &str, offender: bool| {
            let config = ClientConfig {
                name: name.into(),
                crypto: CryptoMode::Required,
                ..ClientConfig::default()
            };
            let client = NetClient::connect(
                UdpTransport::bind(localhost()).expect("bind client"),
                addr,
                config,
                &(),
            )
            .expect("join fits");
            let mut world = World::new();
            world.insert_resource(NetMode::Client);
            register(&mut world, offender);
            regia_net::runtime::ensure_resources(&mut world);
            Peer {
                client,
                world,
                bridge: ClientBridge::new(),
                gameplay: Schedule::default(),
                peer: PeerId(0),
            }
        };
        let a = peer("a", false);
        let b = peer("b", true);
        let mut rig = Self {
            server,
            world,
            bridge: ServerBridge::new(),
            gameplay: server_gameplay,
            a,
            b,
            now: 0,
            events: Vec::new(),
        };
        for _ in 0..300 {
            if rig.a.client.is_connected() && rig.b.client.is_connected() && rig.node().peers().len() == 2 {
                break;
            }
            rig.step();
        }
        assert!(
            rig.a.client.is_connected() && rig.b.client.is_connected(),
            "both clients connect over loopback UDP"
        );
        assert!(
            rig.a.client.is_sealed() && rig.b.client.is_sealed(),
            "both links are encrypted"
        );
        rig.a.peer = rig.a.client.peer_id().expect("a peer id");
        rig.b.peer = rig.b.client.peer_id().expect("b peer id");
        assert!(rig.server.peer_sealed(rig.a.peer) && rig.server.peer_sealed(rig.b.peer));
        assert_eq!(rig.node().peers().len(), 2, "both peers joined the runtime");
        rig.events.clear();
        Some(rig)
    }

    fn node(&self) -> &NetNode {
        self.world.resource::<NetNode>()
    }

    fn step(&mut self) {
        self.now += STEP_MS;
        let sent = self.a.client.totals().datagrams_out + self.b.client.totals().datagrams_out;
        for _ in 0..PUMP_SPINS {
            self.server.receive(self.now);
            if self.server.totals().datagrams_in >= sent {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            self.server.totals().datagrams_in >= sent,
            "loopback delivered {} of {} client datagrams",
            self.server.totals().datagrams_in,
            sent
        );
        let events = self.bridge.pump_in(&mut self.world, &mut self.server);
        self.events.extend(events);
        run_net_tick(&mut self.world, &mut self.gameplay);
        self.bridge.pump_out(&mut self.world, &mut self.server);
        self.server.flush(self.now);

        // Best effort: a client that ended stops counting.
        let sent = self.server.totals().datagrams_out;
        for _ in 0..PUMP_SPINS {
            self.a.client.receive(self.now);
            self.b.client.receive(self.now);
            if self.a.client.totals().datagrams_in + self.b.client.totals().datagrams_in >= sent {
                break;
            }
            std::thread::yield_now();
        }
        for p in [&mut self.a, &mut self.b] {
            let _ = p.bridge.pump_in(&mut p.world, &mut p.client);
            run_net_tick(&mut p.world, &mut p.gameplay);
            p.bridge.pump_out(&mut p.world, &mut p.client);
            p.client.flush(self.now);
        }
    }

    fn left(&self, peer: PeerId) -> Option<DisconnectReason> {
        self.events.iter().find_map(|e| match e {
            ServerEvent::PeerLeft { peer: p, reason } if *p == peer => Some(*reason),
            _ => None,
        })
    }
}

fn send(world: &mut World, f: impl FnOnce(&mut NetSender) + Send + Sync + 'static) {
    let mut once = Some(f);
    world
        .run_system_once(move |mut net: NetSender| {
            if let Some(f) = once.take() {
                f(&mut net);
            }
        })
        .expect("system runs");
}

fn register_kinds(world: &mut World, offender: bool) {
    register_net_message::<Ping>(world);
    if offender {
        register_net_message::<Forged>(world);
    } else {
        register_net_message::<Flash>(world);
    }
}

/// Item 4: a client that sends a server-only message type (here a broadcast) is disconnected with
/// `protocol_error` and named (`server_only_message`), never just counted; the honest client in the
/// same session keeps its link and its own to-server messages are delivered.
#[test]
fn a_server_only_kind_from_a_client_is_a_protocol_error() {
    let Some(mut rig) = Rig::start(register_kinds, Schedule::default()) else {
        return;
    };
    let (a, b) = (rig.a.peer, rig.b.peer);
    send(&mut rig.a.world, |net| {
        net.send_session_to_server(Ping { n: 1 }).expect("queued")
    });
    send(&mut rig.b.world, |net| {
        net.send_session_to_server(Forged { n: 2 }).expect("queued")
    });
    for _ in 0..10 {
        rig.step();
    }
    assert_eq!(
        rig.b.world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError),
        "the sender of a server-only type is disconnected with a protocol error"
    );
    assert!(!rig.b.client.is_connected());
    assert_eq!(rig.node().peers(), vec![a], "only the offender left the runtime");
    assert!(rig.left(b).is_some(), "the endpoint removed the offender");
    assert_eq!(rig.left(a), None);
    assert_eq!(named(&mut rig.world), vec![(b, "server_only_message")]);
    let registry = rig.world.resource::<NetMessageRegistry>();
    let ping = usize::from(registry.index_of::<Ping>().expect("ping registered"));
    let flash = usize::from(registry.index_of::<Flash>().expect("flash registered"));
    assert_eq!(
        registry.counters[ping].delivered, 1,
        "the honest client's message arrived"
    );
    assert_eq!(
        registry.counters[flash].delivered, 0,
        "the forged broadcast never ran"
    );
    assert!(rig.a.client.is_connected());
    assert_eq!(rig.a.world.resource::<NetNode>().closed, None);

    // The honest client keeps working after the offender is gone.
    send(&mut rig.a.world, |net| {
        net.send_session_to_server(Ping { n: 3 }).expect("queued")
    });
    for _ in 0..5 {
        rig.step();
    }
    let registry = rig.world.resource::<NetMessageRegistry>();
    assert_eq!(registry.counters[ping].delivered, 2);
}

/// KI-NT-024 (ADR-10077): the runtime's refusals reach a bridge host as `ServerEvent::Rejected`,
/// once each and before the `PeerLeft` a fatal one causes, so the host reads one stream; the
/// node still lists them for a host that takes them itself. Base: no `Rejected` event named
/// `server_only_message` (the runtime's refusals were only in `NetNode::take_rejections`).
#[test]
fn runtime_refusals_are_rejected_events_of_the_bridge() {
    let Some(mut rig) = Rig::start(register_kinds, Schedule::default()) else {
        return;
    };
    let b = rig.b.peer;
    send(&mut rig.b.world, |net| {
        net.send_session_to_server(Forged { n: 2 }).expect("queued")
    });
    for _ in 0..10 {
        rig.step();
    }
    // The detail the runtime recorded (the server's name for the message type at that index).
    let detail = rig
        .node()
        .rejections()
        .find(|r| r.peer == b)
        .map(|r| r.detail)
        .expect("the runtime refused it");
    let rejected: Vec<usize> = rig
        .events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            ServerEvent::Rejected {
                peer,
                kind: RejectKind::ServerOnlyMessage,
                error,
            } if *peer == b => {
                assert_eq!(*error, WireError::Invalid(detail), "the error carries the detail");
                Some(i)
            }
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 1, "one event per refusal");
    let left = rig
        .events
        .iter()
        .position(|e| matches!(e, ServerEvent::PeerLeft { peer, .. } if *peer == b))
        .expect("the offender left");
    assert!(rejected[0] < left, "the refusal comes before the leave it caused");
    assert_eq!(
        named(&mut rig.world),
        vec![(b, "server_only_message")],
        "still on the node"
    );
}

#[cfg(feature = "voice")]
mod voice {
    use super::*;
    use regia_audio::voice_chat::{
        FRAME_TIMESTAMP_STEP, VoiceChatSettings, VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality,
    };
    use regia_net::server::RejectKind;
    use regia_net::voice::*;

    fn register_voice(world: &mut World, _offender: bool) {
        register_voice_messages(world);
        register_net_message::<Ping>(world);
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: VoiceQuality::Narrow,
            ..VoiceChatSettings::default()
        };
        world.insert_resource(VoiceRelay::new(VoiceRelaySettings::default(), &settings));
    }

    fn valid_packet(seq: u16) -> Vec<u8> {
        let payload = [1u8; 4];
        let packet = VoicePacket {
            flags: VoiceFlags::default(),
            sender: 0,
            channel: 0,
            seq,
            timestamp: u32::from(seq) * FRAME_TIMESTAMP_STEP,
            quality: VoiceQuality::Narrow,
            codec: VoiceCodecId::Pcm,
            payload: &payload,
        };
        let mut out = Vec::new();
        packet.write(&mut out);
        out
    }

    /// Item 2: every voice packet that does not parse is charged to its sender: it spends a token of
    /// the sender's rate bucket, scores `VoiceMalformed`'s weight (16 points, ADR-8666) and is named
    /// (`voice_malformed`), so one bad packet a tick drops the sender with `malformed_packet` in
    /// about a second. The honest client, sending valid voice in the same ticks, is never charged
    /// and stays connected.
    #[test]
    fn malformed_voice_is_charged_to_its_sender_until_it_is_dropped() {
        let mut gameplay = Schedule::default();
        gameplay.add_systems(voice_server_system);
        let Some(mut rig) = Rig::start(register_voice, gameplay) else {
            return;
        };
        let (a, b) = (rig.a.peer, rig.b.peer);
        let mut ticks = 0u32;
        let (mut early, mut early_points_a) = (Vec::new(), None);
        while ticks < 1_300 && rig.left(b).is_none() {
            let seq = ticks as u16;
            send(&mut rig.a.world, move |net| {
                net.send_session_to_server(VoiceUp {
                    packets: vec![valid_packet(seq)],
                })
                .expect("queued");
            });
            if rig.b.client.is_connected() {
                send(&mut rig.b.world, |net| {
                    net.send_session_to_server(VoiceUp {
                        packets: vec![vec![0xEE; 3]],
                    })
                    .expect("queued");
                });
            }
            rig.step();
            ticks += 1;
            if ticks == 10 {
                early = named(&mut rig.world);
                early_points_a = malformed_points(&rig.world, a);
            }
        }
        assert_eq!(
            rig.left(b),
            Some(DisconnectReason::MalformedPacket),
            "{ticks} ticks of malformed voice did not drop the sender (relay {:?})",
            rig.world.resource::<VoiceRelay>().stats()
        );
        assert!(
            !early.is_empty() && early.iter().all(|r| *r == (b, "voice_malformed")),
            "every bad packet is named and names the offender: {early:?}"
        );
        assert_eq!(early_points_a, Some(0), "a is never charged");
        // ADR-8666: each refusal weighs 16 (it was one point, so this bound was the limit itself),
        // and the leak of 4 points a second takes about one more refusal over 64 ticks.
        let weight = RejectKind::VoiceMalformed.malformed_weight();
        assert!(
            ticks >= score_limit() / weight,
            "no fewer than limit / weight refusals drop a peer (after {ticks})"
        );
        assert!(
            ticks <= 2 * 60,
            "a bad packet a tick drops the sender within about 2 s (after {ticks})"
        );
        let stats = rig.world.resource::<VoiceRelay>().stats();
        assert!(
            stats.bad_packets >= u64::from(score_limit() / weight),
            "{stats:?}"
        );
        assert_eq!(rig.left(a), None, "the honest client is never dropped");
        assert!(rig.a.client.is_connected());
        assert_eq!(rig.node().peers(), vec![a]);
        assert!(named(&mut rig.world).iter().all(|r| r.0 == b));
    }

    /// Item 2, the bucket: a packet that does not parse spends a token of its sender's bucket like
    /// a valid one, so garbage cannot ride for free next to speech: after a burst of garbage the
    /// sender's next valid packet is over its rate.
    #[test]
    fn a_malformed_voice_packet_spends_a_token_of_the_senders_bucket() {
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: VoiceQuality::Narrow,
            ..VoiceChatSettings::default()
        };
        let mut relay = VoiceRelay::new(VoiceRelaySettings::default(), &settings);
        relay.add_peer(PeerId(1)).expect("slot");
        let burst = VOICE_BURST as usize;
        for _ in 0..burst {
            relay.route(PeerId(1), &[0xEE; 3], 1_000);
        }
        let before = relay.stats();
        assert_eq!(before.bad_packets, burst as u64);
        relay.route(PeerId(1), &valid_packet(0), 1_000);
        let after = relay.stats();
        assert_eq!(
            after.rate_limited,
            before.rate_limited + 1,
            "the burst of garbage spent the bucket, so the valid packet is over the rate: {after:?}"
        );
        assert_eq!(voice_rejected(&mut relay), vec![PeerId(1); burst]);
    }

    /// A voice packet that does not parse.
    fn garbage() -> Vec<u8> {
        vec![0xEE; 3]
    }

    /// Review item 4 (ADR-8438, ADR-8666): no pass earns credit, so a garbage flooder that is
    /// charged on one tick and over its rate on the next still climbs. At 120 ticks a second the
    /// voice bucket (60 packets a second) admits every other packet of a sender that sends one a
    /// tick: 60 refusals of 16 points a second against the 4-point leak cross 1,024 in about
    /// 1.1 s (plus the burst's head start). The honest speaker in the same session, also over its
    /// rate on every other tick, is never charged and stays connected.
    #[test]
    fn a_voice_flood_at_twice_the_bucket_rate_climbs_to_the_limit() {
        let mut gameplay = Schedule::default();
        gameplay.add_systems(voice_server_system);
        let Some(mut rig) = Rig::start(register_voice, gameplay) else {
            return;
        };
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 120.0;
        rig.world.insert_resource(fixed);
        // 120 ticks a second against VOICE_RATE_PER_SECOND (60): the bucket admits every other
        // packet once the burst is spent.
        let (a, b) = (rig.a.peer, rig.b.peer);
        let mut ticks = 0u32;
        let mut peak = 0u32;
        while ticks < 3_000 && rig.left(b).is_none() {
            let seq = ticks as u16;
            send(&mut rig.a.world, move |net| {
                net.send_session_to_server(VoiceUp {
                    packets: vec![valid_packet(seq)],
                })
                .expect("queued");
            });
            if rig.b.client.is_connected() {
                send(&mut rig.b.world, |net| {
                    net.send_session_to_server(VoiceUp {
                        packets: vec![garbage()],
                    })
                    .expect("queued");
                });
            }
            rig.step();
            ticks += 1;
            peak = peak.max(malformed_points(&rig.world, b).unwrap_or(0));
            assert_eq!(
                malformed_points(&rig.world, a).unwrap_or(0),
                0,
                "the honest speaker is never charged (tick {ticks})"
            );
        }
        let stats = rig.world.resource::<VoiceRelay>().stats();
        assert!(
            peak >= score_limit() / 2,
            "the flooder's score climbs at half the tick rate: peak {peak} ({stats:?})"
        );
        assert!(
            ticks <= 2 * 120,
            "garbage at twice the bucket rate is dropped within about 2 s: {ticks} ticks at 120 Hz ({stats:?})"
        );
        assert_eq!(
            rig.left(b),
            Some(DisconnectReason::MalformedPacket),
            "{ticks} ticks of garbage at twice the bucket rate did not drop the sender ({stats:?})"
        );
        assert!(
            stats.rate_limited > 0,
            "the bucket refused some packets: {stats:?}"
        );
        assert_eq!(rig.left(a), None, "the honest client is never dropped");
        assert!(rig.a.client.is_connected());
        assert_eq!(rig.node().peers(), vec![a]);
    }

    /// ADR-8666 (replacing ADR-8438's pass credit): the runtime score is the server's leaky bucket.
    /// A burst of garbage scores 16 a packet, a second of silence leaks 4 points, and clean
    /// messages earn nothing back beyond the leak, so bad traffic mixed with good still adds up.
    #[test]
    fn a_burst_leaks_4_points_a_second_and_clean_traffic_earns_nothing() {
        let mut gameplay = Schedule::default();
        gameplay.add_systems(voice_server_system);
        let Some(mut rig) = Rig::start(register_voice, gameplay) else {
            return;
        };
        let (a, b) = (rig.a.peer, rig.b.peer);
        let burst = VOICE_BURST as usize;
        send(&mut rig.b.world, move |net| {
            net.send_session_to_server(VoiceUp {
                packets: vec![garbage(); burst],
            })
            .expect("queued");
        });
        for _ in 0..5 {
            rig.step();
        }
        let weight = RejectKind::VoiceMalformed.malformed_weight();
        let full = burst as u32 * weight;
        // A few ticks of leak (under a point) may have passed since the charge.
        let charged = malformed_points(&rig.world, b).unwrap_or(0);
        assert!(
            (full - 1..=full).contains(&charged),
            "each packet of the burst scores {weight}: {charged} of {full} ({:?})",
            rig.world.resource::<VoiceRelay>().stats()
        );
        for _ in 0..60 {
            rig.step();
        }
        let silent = malformed_points(&rig.world, b).unwrap_or(0);
        assert!(
            (charged - 5..=charged - 3).contains(&silent),
            "a second of silence leaks about 4 points: {charged} -> {silent}"
        );
        for n in 0..4 {
            send(&mut rig.b.world, move |net| {
                net.send_session_to_server(Ping { n }).expect("queued")
            });
            rig.step();
        }
        for _ in 0..3 {
            rig.step();
        }
        let after = malformed_points(&rig.world, b).unwrap_or(0);
        assert!(
            (silent - 1..=silent).contains(&after),
            "clean messages earn nothing; 7 ticks leak under one point: {silent} -> {after}"
        );
        assert_eq!(malformed_points(&rig.world, a), Some(0));
        assert_eq!(rig.left(b), None);
    }

    /// Review item 24 (ADR-8438): a voice control message with an unknown kind, or a block whose
    /// account is over `MAX_ACCOUNT_BYTES`, is malformed: it is charged to its sender and named
    /// (`voice_malformed`), never ignored for free. The honest client's valid join is not.
    #[test]
    fn malformed_voice_control_is_charged_and_named() {
        let mut gameplay = Schedule::default();
        gameplay.add_systems(voice_server_system);
        let Some(mut rig) = Rig::start(register_voice, gameplay) else {
            return;
        };
        let (a, b) = (rig.a.peer, rig.b.peer);
        send(&mut rig.a.world, |net| {
            net.send_session_to_server(VoiceCtrl {
                kind: 1,
                id: 0,
                on: true,
                account: String::new(),
            })
            .expect("queued");
        });
        send(&mut rig.b.world, |net| {
            net.send_session_to_server(VoiceCtrl {
                kind: 99,
                id: 0,
                on: true,
                account: String::new(),
            })
            .expect("queued");
            net.send_session_to_server(VoiceCtrl {
                kind: 4,
                id: 0,
                on: true,
                account: "x".repeat(MAX_ACCOUNT_BYTES + 1),
            })
            .expect("queued");
        });
        for _ in 0..10 {
            rig.step();
        }
        assert_eq!(
            named(&mut rig.world),
            vec![(b, "voice_malformed"), (b, "voice_malformed")],
            "both malformed controls are named and name their sender"
        );
        // Two refusals of 16 (ADR-8666; one point each before); ten ticks leak under one point.
        let weight = RejectKind::VoiceMalformed.malformed_weight();
        assert!(
            malformed_points(&rig.world, b).is_some_and(|p| (2 * weight - 1..=2 * weight).contains(&p)),
            "{:?}",
            malformed_points(&rig.world, b)
        );
        assert_eq!(malformed_points(&rig.world, a), Some(0));
        assert_eq!(rig.left(a), None);
        assert_eq!(rig.left(b), None, "a malformed control scores; it is not fatal");
    }

    /// Review item 26 (ADR-8438): a host that routes packets itself and never takes the rejected
    /// senders cannot grow the list without bound: the relay keeps the newest entries and counts
    /// the dropped ones; every packet stays in `bad_packets`.
    #[test]
    fn untaken_voice_rejections_are_bounded() {
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: VoiceQuality::Narrow,
            ..VoiceChatSettings::default()
        };
        let mut relay = VoiceRelay::new(VoiceRelaySettings::default(), &settings);
        relay.add_peer(PeerId(1)).expect("slot");
        let kept = kept_voice_rejects();
        let extra = 25;
        // One packet a second: always inside the bucket, so each one is parsed and refused.
        for i in 0..(kept + extra) as u64 {
            relay.route(PeerId(1), &garbage(), i * 1_000);
        }
        assert_eq!(relay.stats().bad_packets, (kept + extra) as u64);
        assert_eq!(voice_rejected_dropped(&relay), extra as u64);
        assert_eq!(voice_rejected(&mut relay).len(), kept);
        assert!(voice_rejected(&mut relay).is_empty());
    }

    // BEGIN new-api: the fail-before run on the base replaces this block with stubs.
    fn kept_voice_rejects() -> usize {
        MAX_KEPT_VOICE_REJECTS
    }

    fn voice_rejected_dropped(relay: &VoiceRelay) -> u64 {
        relay.rejected_dropped()
    }

    fn voice_rejected(relay: &mut VoiceRelay) -> Vec<PeerId> {
        relay.take_rejected()
    }

    fn score_limit() -> u32 {
        regia_net::server::MALFORMED_SCORE_LIMIT
    }

    fn malformed_points(world: &World, peer: PeerId) -> Option<u32> {
        world.resource::<NetNode>().peer_malformed_points(peer)
    }
    // END new-api
}

// BEGIN new-api: the fail-before run on the base replaces this block with a stub.
/// `(peer, kind name)` of every refusal the runtime recorded since the last call, taking them.
fn named(world: &mut World) -> Vec<(PeerId, &'static str)> {
    world
        .resource_mut::<NetNode>()
        .take_rejections()
        .into_iter()
        .map(|r| (r.peer, r.kind.as_str()))
        .collect()
}
// END new-api
