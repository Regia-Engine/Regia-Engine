// SPDX-License-Identifier: MPL-2.0
//! Review rows 1 and 5: ECS bridge traffic on sealed, delayed/reordered links.
use std::net::SocketAddr;

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::codec::{CodecContext, ReplicateCodec};
use regia_net::conditioner::ConditionerConfig;
use regia_net::message::{Channel as EcsChannel, register_net_message};
use regia_net::predict::{InputStream, add_net_input};
use regia_net::runtime::{Datagram, NetNode, ensure_resources, net_receive, run_net_tick};
use regia_net::*;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.ack_review";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Clone, Debug, Default, PartialEq, Replicate)]
struct Pad(u8);
impl NetInput for Pad {}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Event {
    #[replicate(max_len = 64)]
    bytes: Vec<u8>,
}
impl NetMessage for Event {
    const CHANNEL: EcsChannel = EcsChannel::Unreliable;
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for Event {}

struct Rig {
    server: NetServer<Game, LoopbackSocket>,
    client: NetClient<Game, LoopbackSocket>,
    sw: World,
    cw: World,
    sb: ServerBridge,
    cb: ClientBridge,
    now: u64,
}

impl Rig {
    fn new(window: usize) -> Self {
        let net = LoopbackNet::new();
        let addr = SocketAddr::from(([127, 0, 0, 1], 7000));
        // Sustained reliable game traffic past the class message caps (spec 120 § 4.5 A2,
        // ADR-8407): the rig measures the transport, so the caps are off.
        let link = LinkConfig {
            max_reliable_in_flight: window,
            message_caps: false,
            ..Default::default()
        };
        let conditioner = ConditionerConfig {
            enabled: true,
            seed: 79,
            delay_ms: 125,
            reorder: 0.05,
            reorder_max_ms: 40,
            ..Default::default()
        };
        let server = NetServer::with_secret(
            net.bind(addr).expect("bind"),
            ServerConfig {
                link: link.clone(),
                conditioner: conditioner.clone(),
                crypto: CryptoMode::Required,
                ..Default::default()
            },
            [33; 32],
        );
        let client = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
            addr,
            ClientConfig {
                link,
                conditioner,
                crypto: CryptoMode::Required,
                ..Default::default()
            },
            &(),
        )
        .expect("connect");
        let (mut sw, mut cw) = (World::new(), World::new());
        sw.insert_resource(NetMode::DedicatedServer);
        cw.insert_resource(NetMode::Client);
        for w in [&mut sw, &mut cw] {
            register_net_message::<Event>(w);
            add_net_input::<Pad>(w).expect("input");
            ensure_resources(w);
        }
        let mut r = Self {
            server,
            client,
            sw,
            cw,
            sb: ServerBridge::new(),
            cb: ClientBridge::new(),
            now: 0,
        };
        for _ in 0..240 {
            r.step(true);
        }
        assert!(r.client.is_connected());
        assert!(r.client.is_sealed());
        r
    }

    fn step(&mut self, runtime: bool) {
        self.now += 17;
        self.server.receive(self.now);
        self.sb.pump_in(&mut self.sw, &mut self.server);
        self.client.receive(self.now);
        self.cb.pump_in(&mut self.cw, &mut self.client);
        if runtime {
            run_net_tick(&mut self.sw, &mut Schedule::default());
            run_net_tick(&mut self.cw, &mut Schedule::default());
        }
        self.sb.pump_out(&mut self.sw, &mut self.server);
        self.cb.pump_out(&mut self.cw, &mut self.client);
        self.server.flush(self.now);
        self.client.flush(self.now);
    }

    fn backlogs(&mut self) {
        let peer = self.client.peer_id().expect("peer");
        for _ in 0..32 {
            self.sw.resource_mut::<NetNode>().outbox.push(Datagram {
                peer,
                channel: EcsChannel::ReliableOrdered,
                bytes: vec![0; 1100],
            });
            self.cw.resource_mut::<NetNode>().outbox.push(Datagram {
                peer: PeerId::SERVER,
                channel: EcsChannel::ReliableOrdered,
                bytes: vec![0; 1100],
            });
        }
        self.sb.pump_out(&mut self.sw, &mut self.server);
        self.cb.pump_out(&mut self.cw, &mut self.client);
        assert!(self.sb.backlog(peer) > 0);
    }
}

#[test]
fn forty_unreliable_game_events_per_tick_survive_large_batches() {
    let mut r = Rig::new(1024);
    let peer = r.client.peer_id().expect("peer");
    let start = r.cw.resource::<regia_net::message::NetMessageRegistry>().counters[0].delivered;
    for _ in 0..120 {
        r.sw.run_system_once(move |mut net: NetSender| {
            for _ in 0..40 {
                net.send_session_to_peer(peer, Event { bytes: vec![7; 40] })
                    .expect("queue event");
            }
        })
        .expect("send system");
        r.step(true);
    }
    for _ in 0..180 {
        r.step(true);
    }
    let delivered = r.cw.resource::<regia_net::message::NetMessageRegistry>().counters[0].delivered - start;
    println!(
        "BRIDGE offered=4800 delivered={delivered} shed={} rtt>=250ms",
        r.sb.unreliable_shed(peer)
    );
    assert_eq!(delivered, 4800, "oversize game batches must not be silently shed");
    assert_eq!(r.sb.unreliable_shed(peer), 0);
}

#[test]
fn ecs_input_is_stored_while_reliable_backlogs_are_full() {
    let mut r = Rig::new(2);
    r.backlogs();
    let peer = r.client.peer_id().expect("peer");
    let tick = r.sw.resource::<NetNode>().tick.0 + 20;
    let mut w = BitWriter::new();
    w.write_bits(5, 8); // TAG_INPUT, the production ECS input frame layout.
    w.write_varint(u64::from(tick));
    w.write_varint(1);
    w.write_bits(0, 3);
    w.write_varint(1);
    w.write_varint(u64::from(tick));
    w.write_bool(false); // no view
    w.write_bool(false); // no trusted position
    Pad(9).encode(&mut w, &CodecContext::detached());
    r.cw.resource_mut::<NetNode>().outbox.push(Datagram {
        peer: PeerId::SERVER,
        channel: EcsChannel::Unreliable,
        bytes: w.finish(),
    });
    for _ in 0..12 {
        r.step(false);
        // Receive on each simulation tick so the input packet rate limit has its real cadence.
        // Synthetic bulk padding exercises the bridge, not the game's frame decoder.
        r.sw.resource_mut::<NetNode>()
            .inbox
            .retain(|d| d.bytes.first() == Some(&5));
        net_receive(&mut r.sw);
        let next = r.sw.resource::<NetNode>().tick.next();
        r.sw.resource_mut::<NetNode>().tick = next;
    }
    let buffered =
        r.sw.resource::<InputStream<Pad>>()
            .buffered_ticks(NetPlayerId { peer, local: 0 });
    println!(
        "ECS_INPUT buffered={} server_backlog={}",
        buffered.len(),
        r.sb.backlog(peer)
    );
    assert!(
        buffered.contains(&NetTick(tick)),
        "ECS input bypasses reliable backpressure"
    );
    assert!(r.sb.backlog(peer) > 0);
}

#[test]
fn ecs_input_ack_bypasses_the_servers_reliable_backlog() {
    let mut r = Rig::new(2);
    r.backlogs();
    let peer = r.client.peer_id().expect("peer");
    let mut w = BitWriter::new();
    w.write_bits(6, 8); // TAG_INPUT_ACK, no player rows needed for this transport test.
    w.write_varint(u64::from(r.sw.resource::<NetNode>().tick.0));
    w.write_varint(0);
    let bytes = w.finish();
    r.sw.resource_mut::<NetNode>().outbox.push(Datagram {
        peer,
        channel: EcsChannel::Unreliable,
        bytes: bytes.clone(),
    });
    for _ in 0..12 {
        r.step(false);
    }
    assert!(
        r.cw.resource::<NetNode>().inbox.iter().any(|d| d.bytes == bytes),
        "ECS input acknowledgment bypasses reliable backpressure"
    );
    assert!(r.sb.backlog(peer) > 0);
}

#[test]
fn oversized_essential_frames_are_shed_without_partial_encodings() {
    let mut r = Rig::new(1024);
    let peer = r.client.peer_id().expect("peer");
    let server_bad = r.server.totals().malformed;
    let client_bad = r.client.totals().malformed;
    for _ in 0..12 {
        let mut input = vec![0; regia_net::bridge::FRAME_PART_BYTES + 1];
        input[0] = 5; // TAG_INPUT: larger than a NetFrame can encode.
        let mut ack = input.clone();
        ack[0] = 6; // TAG_INPUT_ACK: same bound on the server side.
        r.cw.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: EcsChannel::Unreliable,
            bytes: input,
        });
        r.sw.resource_mut::<NetNode>().outbox.push(Datagram {
            peer,
            channel: EcsChannel::Unreliable,
            bytes: ack,
        });
        r.step(false);
    }
    for _ in 0..100 {
        r.step(false);
    }
    println!(
        "OVERSIZE_INPUT server_bad={} client_bad={} connected={}",
        r.server.totals().malformed - server_bad,
        r.client.totals().malformed - client_bad,
        r.client.is_connected()
    );
    assert_eq!(r.server.totals().malformed, server_bad);
    assert_eq!(r.client.totals().malformed, client_bad);
    assert!(r.client.is_connected());
    assert_eq!(r.sb.unreliable_shed(peer), 12);
}

#[test]
fn sustained_reliable_game_traffic_does_not_overflow_at_250ms_rtt() {
    let mut r = Rig::new(1024);
    let peer = r.client.peer_id().expect("peer");
    r.cw.resource_mut::<NetNode>().inbox.clear();
    let mut delivered = 0;
    let mut peak_backlog = 0;
    let mut offered = 0;
    for tick in 0..2_700 {
        if tick < 2_400 {
            for _ in 0..10 {
                r.sw.resource_mut::<NetNode>().outbox.push(Datagram {
                    peer,
                    channel: EcsChannel::ReliableOrdered,
                    bytes: vec![0xA5; 1100],
                });
                offered += 1;
            }
        }
        r.step(false);
        peak_backlog = peak_backlog.max(r.sb.backlog_bytes(peer));
        delivered += std::mem::take(&mut r.cw.resource_mut::<NetNode>().inbox)
            .iter()
            .filter(|d| d.bytes == vec![0xA5; 1100])
            .count();
        r.sw.resource_mut::<NetNode>().inbox.clear();
    }
    println!(
        "BULK offered={offered} delivered={delivered} payload_mbps={} peak_backlog={peak_backlog} connected={}",
        delivered as f64 * 1100.0 * 8.0 / 40.8e6,
        r.client.is_connected()
    );
    assert!(
        r.client.is_connected(),
        "a flowing high-RTT link must not hit ReliableOverflow"
    );
    assert_eq!(delivered, offered, "all reliable frames delivered");
    assert!(
        peak_backlog < 1024 * 1024,
        "the backlog stays bounded in steady state"
    );
}
