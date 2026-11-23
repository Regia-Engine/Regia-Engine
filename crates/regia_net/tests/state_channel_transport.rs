// SPDX-License-Identifier: MPL-2.0
//! The state channel over the real endpoints (`NetServer` / `NetClient`, `regia_net::bridge`) on
//! the in-memory network, with the outbound network conditioner on: negotiation happens through
//! real `ACK` frames, and a client on a bad link converges (spec 72 § 4, ADR-1950). Time is a step
//! counter (16 ms per step); nothing sleeps.

use std::net::SocketAddr;

use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::conditioner::ConditionerConfig;
use regia_net::replication::{ClientReplication, ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{NetEntities, NetNode, run_net_tick};
use regia_net::*;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.state";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp(u32);

fn setup(world: &mut World, mode: NetMode) {
    world.insert_resource(mode);
    register_replicated::<Hp>(world, ReplicateOptions::default());
    regia_net::runtime::ensure_resources(world);
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn a_conditioned_link_converges_on_the_default_settings() {
    for profile in ["wan_stress", "bad", "burst"] {
        let net = LoopbackNet::new();
        let conditioner = ConditionerConfig::from_profile(profile).expect("profile");
        let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
            net.bind(addr(7000)).expect("bind"),
            ServerConfig {
                conditioner,
                ..ServerConfig::default()
            },
            [3; 32],
        );
        let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
            net.bind(addr(0)).expect("bind"),
            addr(7000),
            ClientConfig::default(),
            &(),
        )
        .expect("join fits");
        let (mut sw, mut cw) = (World::new(), World::new());
        setup(&mut sw, NetMode::DedicatedServer);
        setup(&mut cw, NetMode::Client);
        let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
        let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
        let ents: Vec<Entity> = (0..40u32)
            .map(|i| sw.spawn((Replicated::default(), Hp(i))).id())
            .collect();
        let mut now = 1000;
        let mut step = |sw: &mut World, cw: &mut World| {
            now += 16;
            server.receive(now);
            sb.pump_in(sw, &mut server);
            run_net_tick(sw, &mut gs);
            sb.pump_out(sw, &mut server);
            server.flush(now);
            client.receive(now);
            cb.pump_in(cw, &mut client);
            run_net_tick(cw, &mut gc);
            cb.pump_out(cw, &mut client);
            client.flush(now);
        };
        // 15 s of changes (past the burst profile's blackout), then 3 s of quiet.
        for t in 0..900u32 {
            for (i, e) in ents.iter().enumerate() {
                if (t as usize + i).is_multiple_of(7) {
                    sw.get_mut::<Hp>(*e).expect("hp").0 = 1000 + t;
                }
            }
            step(&mut sw, &mut cw);
        }
        for _ in 0..190 {
            step(&mut sw, &mut cw);
        }
        let peer = sw.resource::<NetNode>().peers()[0];
        let link = sw.resource::<ServerReplication>().state_link(peer).expect("link");
        assert_eq!(link.wire, ConnWire::Acked, "{profile}");
        assert!(link.counters.packets_sent > 200, "{profile}");
        assert!(cw.resource::<ClientReplication>().state.active(), "{profile}");
        for e in &ents {
            let id = *sw.get::<NetId>(*e).expect("id");
            let copy = cw.resource::<NetEntities>().entity(id).expect("copy");
            assert_eq!(sw.get::<Hp>(*e), cw.get::<Hp>(copy), "{profile} {id:?}");
        }
    }
}

/// The bridge supplies each sealed peer's actual cap to assembly, including a custom small
/// transport packet size. Every ordinary state frame stays unreliable and the client converges.
#[test]
fn sealed_state_frames_obey_the_actual_connection_cap() {
    for packet_cap in [256, 1200] {
        let net = LoopbackNet::new();
        let link = LinkConfig {
            max_packet_bytes: packet_cap,
            ..LinkConfig::default()
        };
        let max_frame = regia_net::connection::Connection::new(link.clone())
            .max_unreliable_bytes(regia_net::crypto::SEALED_RESERVED_HEADER_BYTES)
            - 4;
        let mut server: NetServer<Game, _> = NetServer::with_secret(
            net.bind(addr(7000)).expect("bind"),
            ServerConfig {
                link: link.clone(),
                crypto: CryptoMode::Required,
                ..ServerConfig::default()
            },
            [3; 32],
        );
        let mut client: NetClient<Game, _> = NetClient::connect(
            net.bind(addr(0)).expect("bind"),
            addr(7000),
            ClientConfig {
                link,
                crypto: CryptoMode::Required,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("join");
        let (mut sw, mut cw) = (World::new(), World::new());
        setup(&mut sw, NetMode::DedicatedServer);
        setup(&mut cw, NetMode::Client);
        sw.resource_mut::<NetSettings>().replication.mtu_bytes = 1400;
        let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
        let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
        let entities: Vec<_> = (0..100)
            .map(|i| sw.spawn((Replicated::default(), Hp(i))).id())
            .collect();
        let mut states = 0;
        for step in 0..300u64 {
            let now = step * 16;
            server.receive(now);
            sb.pump_in(&mut sw, &mut server);
            run_net_tick(&mut sw, &mut gs);
            for frame in &sw.resource::<NetNode>().outbox {
                if frame.bytes.first() == Some(&7) {
                    states += 1;
                    assert_eq!(frame.channel, regia_net::message::Channel::Unreliable);
                    assert!(
                        frame.bytes.len() <= max_frame,
                        "{packet_cap}: state frame {} > {max_frame}",
                        frame.bytes.len()
                    );
                }
            }
            sb.pump_out(&mut sw, &mut server);
            server.flush(now);
            client.receive(now);
            cb.pump_in(&mut cw, &mut client);
            run_net_tick(&mut cw, &mut gc);
            cb.pump_out(&mut cw, &mut client);
            client.flush(now);
        }
        assert!(client.is_sealed());
        assert!(states > 100);
        for e in entities {
            let id = *sw.get::<NetId>(e).expect("id");
            let copy = cw.resource::<NetEntities>().entity(id).expect("replicated");
            assert_eq!(sw.get::<Hp>(e), cw.get::<Hp>(copy));
        }
    }
}
