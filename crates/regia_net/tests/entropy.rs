// SPDX-License-Identifier: MPL-2.0
//! The entropy stage end to end (spec 72 § 18, ADR-3315): real `NetServer`/`NetClient` over the
//! in-memory network with the outbound conditioner on, the server configured for `range`: the
//! negotiation happens through real `ACK` frames, entropy-coded state frames converge under loss,
//! duplication and reordering, and the stage's counters show the saving on the real send path.
//! A protocol 2 negotiation (a client whose `ACK` names protocol 2) is gated in the unit tests of
//! `state_channel.rs`; time is a step counter here, nothing sleeps.

use std::net::SocketAddr;

use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::conditioner::ConditionerConfig;
use regia_net::replication::{ClientReplication, ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{NetEntities, NetNode, ensure_resources, run_net_tick};
use regia_net::*;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.entropy";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp(u32);

fn setup(world: &mut World, mode: NetMode, entropy: regia_net::entropy::EntropyMode) {
    world.insert_resource(mode);
    register_replicated::<Hp>(world, ReplicateOptions::default());
    ensure_resources(world);
    world.resource_mut::<NetSettings>().replication.entropy = entropy;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Runs `ticks` of a conditioned two-world session and returns the server's link and the client
/// world for assertions.
fn run(profile: &str, entropy: regia_net::entropy::EntropyMode, ticks: u32) -> (World, World) {
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
    setup(&mut sw, NetMode::DedicatedServer, entropy);
    setup(&mut cw, NetMode::Client, entropy);
    let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
    let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
    let ents: Vec<Entity> = (0..48u32)
        .map(|i| sw.spawn((Replicated::default(), Hp(i))).id())
        .collect();
    let mut now = 1000;
    for t in 0..ticks {
        for (i, e) in ents.iter().enumerate() {
            if (t as usize + i).is_multiple_of(5) {
                sw.get_mut::<Hp>(*e).expect("hp").0 = 500 + t;
            }
        }
        now += 16;
        server.receive(now);
        sb.pump_in(&mut sw, &mut server);
        run_net_tick(&mut sw, &mut gs);
        sb.pump_out(&mut sw, &mut server);
        server.flush(now);
        client.receive(now);
        cb.pump_in(&mut cw, &mut client);
        run_net_tick(&mut cw, &mut gc);
        cb.pump_out(&mut cw, &mut client);
        client.flush(now);
    }
    for _ in 0..240 {
        // Quiet settle: the last change crosses the link before we assert convergence (the
        // interpolation delay and the conditioner's reorder window are in ticks, not seconds).
        now += 16;
        server.receive(now);
        sb.pump_in(&mut sw, &mut server);
        run_net_tick(&mut sw, &mut gs);
        sb.pump_out(&mut sw, &mut server);
        server.flush(now);
        client.receive(now);
        cb.pump_in(&mut cw, &mut client);
        run_net_tick(&mut cw, &mut gc);
        cb.pump_out(&mut cw, &mut client);
        client.flush(now);
    }
    // The entities must have converged whatever the wire did.
    for e in &ents {
        let id = *sw.get::<NetId>(*e).expect("id");
        let copy = cw.resource::<NetEntities>().entity(id).expect("copy");
        assert_eq!(
            sw.get::<Hp>(*e),
            cw.get::<Hp>(copy),
            "{profile}: server and client disagree"
        );
    }
    (sw, cw)
}

#[test]
fn entropy_negotiates_and_converges_under_loss_reorder_and_duplication() {
    for profile in ["wan_stress", "bad", "burst"] {
        let (sw, cw) = run(profile, regia_net::entropy::EntropyMode::Range, 900);
        let peer = sw.resource::<NetNode>().peers()[0];
        let link = sw.resource::<ServerReplication>().state_link(peer).expect("link");
        assert_eq!(link.wire, ConnWire::Acked, "{profile}");
        assert_eq!(
            link.entropy,
            regia_net::entropy::EntropyMode::Range,
            "{profile}: the v3 client negotiates entropy"
        );
        assert!(link.counters.packets_sent > 100, "{profile}");
        assert!(cw.resource::<ClientReplication>().state.active(), "{profile}");
        assert!(link.entropy_stats.coded > 0, "{profile}: the stage coded frames");
        assert!(
            link.entropy_stats.saved() > 0,
            "{profile}: bytes_in {} bytes_out {}",
            link.entropy_stats.bytes_in,
            link.entropy_stats.bytes_out
        );
    }
}

#[test]
fn off_negotiates_off_and_stays_on_the_protocol_2_wire() {
    let (sw, _cw) = run("wan_stress", regia_net::entropy::EntropyMode::Off, 600);
    let peer = sw.resource::<NetNode>().peers()[0];
    let link = sw.resource::<ServerReplication>().state_link(peer).expect("link");
    assert_eq!(link.wire, ConnWire::Acked);
    assert_eq!(link.entropy, regia_net::entropy::EntropyMode::Off);
    assert_eq!(link.entropy_stats.frames, 0, "the stage never runs");
}

#[test]
fn range_sends_fewer_bytes_than_the_stage_takes_in() {
    for profile in ["wan_stress", "bad"] {
        let (sw, _) = run(profile, regia_net::entropy::EntropyMode::Range, 900);
        let peer = sw.resource::<NetNode>().peers()[0];
        let s = &sw
            .resource::<ServerReplication>()
            .state_link(peer)
            .expect("link")
            .entropy_stats;
        assert_eq!(s.frames, s.coded + s.raw_fallback, "{profile}");
        assert!(s.coded > 0, "{profile}");
        assert!(
            s.bytes_out < s.bytes_in,
            "{profile}: the stage must shrink the real workload: {} vs {}",
            s.bytes_out,
            s.bytes_in
        );
        assert_eq!(s.saved(), s.bytes_in as i64 - s.bytes_out as i64);
    }
}

/// A corrupted or truncated entropy payload is rejected with a decode error, never a panic: the
/// unwrap stage must refuse every truncation and tolerate every mutation of the coded region.
/// (The wire path feeds it authenticated plaintext, so an error means tampering or a bug; the
/// decode-error propagation into a disconnect is the existing malformed-frame path, exercised by
/// the state channel's own tests.)
#[test]
fn corrupted_entropy_payloads_error_never_panic() {
    let body: Vec<u8> = (0..300u32)
        .map(|i| if i.is_multiple_of(3) { 0 } else { (i % 7) as u8 })
        .collect();
    let (coded, true) = regia_net::entropy::code_or_raw(&body) else {
        panic!("the fixture must code");
    };
    for cut in 0..coded.len() {
        assert!(
            regia_net::entropy::decode_coded(&coded[..cut]).is_err() || cut >= coded.len() - 5,
            "truncation at {cut} must not decode cleanly"
        );
    }
    let mut corrupt = coded.clone();
    for i in 0..corrupt.len() {
        corrupt[i] ^= 0xA5;
        let _ = regia_net::entropy::decode_coded(&corrupt);
        corrupt[i] ^= 0xA5;
    }
}
