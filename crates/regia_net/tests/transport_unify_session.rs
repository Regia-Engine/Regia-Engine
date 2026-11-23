// SPDX-License-Identifier: MPL-2.0
//! Review regressions of the timeout policy (spec 120 §§ 10.1-10.2, ADR-7950): the interruption
//! marker cannot outlive its peer or a hand-off, the marker is opt-in for the protocol hash,
//! half-open links get no start grace, and an explicit `timeout_ms` wins over legacy seconds.
//! Virtual time only: every clock is a manual one stepped by fixed amounts.

use regia_app::App;
use regia_ecs::prelude::*;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::prelude::*;
use regia_net::runtime::NetNode;
use regia_net::session::TimeoutPolicy;
use regia_net::{NetInterrupted, NetMode, NetOwner, NetPlayer, NetPlayerId, NetPlugin};
use std::net::SocketAddr;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[derive(Debug)]
struct LinkGame;
impl NetGame for LinkGame {
    type Message = regia_net::bridge::NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.transport_unify_session";
    const PROTOCOL_VERSION: u32 = 1;
}

fn link_app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

/// A joined server/client `NetLink` pair on 10 ms manual clocks. From 2 s on both directions lose
/// every datagram for `outage_ms`. Returns the apps and the client's peer id on the server.
fn linked_pair(net: &LoopbackNet, port: u16, timeout: TimeoutPolicy, outage_ms: u64) -> (App, App, PeerId) {
    let mut server = link_app(NetMode::DedicatedServer);
    let mut client = link_app(NetMode::Client);
    let options = LinkOptions {
        timeout,
        // These rows test the timeout path where a silent peer leaves; with the dormant resume of
        // ADR-8298 (on by default for NetLink) a timeout holds the peer instead, which
        // tests/link_resume.rs covers. `0` turns it off.
        resume_grace_ms: 0,
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 10_000,
        },
        conditioner: ConditionerConfig {
            enabled: true,
            burst_start_ms: 2_000,
            burst_len_ms: outage_ms,
            burst_loss: 1.0,
            ..Default::default()
        },
        ..Default::default()
    };
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<LinkGame>(Box::new(net.bind(addr(port)).expect("bind")), &options)
        .expect("host");
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<LinkGame>(Box::new(net.bind(addr(0)).expect("bind")), addr(port), &options)
        .expect("join");
    for _ in 0..200 {
        server.update();
        client.update();
    }
    assert!(client.world().resource::<NetLink>().is_connected());
    let peer = server.world().resource::<NetNode>().peers()[0];
    (server, client, peer)
}

fn marked(app: &mut App) -> usize {
    let mut q = app.world_mut().query_filtered::<Entity, With<NetInterrupted>>();
    q.iter(app.world()).count()
}

/// Row 9: a peer marked interrupted at 1.5 s that times out keeps no marker on any entity. The
/// game keeps its avatar (no despawn): on the base the avatar stayed frozen for good.
#[test]
fn interrupted_marker_is_cleared_when_the_silent_peer_times_out() {
    let net = LoopbackNet::new();
    let timeout = TimeoutPolicy {
        timeout_ms: 10_000,
        start_grace_ms: 0,
        ..Default::default()
    };
    let (mut server, mut client, peer) = linked_pair(&net, 7781, timeout, 60_000);
    let avatar = server.world_mut().spawn(NetOwner(peer)).id();
    let controller = server
        .world_mut()
        .spawn(NetPlayer {
            id: NetPlayerId { peer, local: 0 },
        })
        .id();
    let mut saw_marker = false;
    for _ in 0..1_500 {
        server.update();
        client.update();
        saw_marker |= server.world().get::<NetInterrupted>(avatar).is_some()
            && server.world().get::<NetInterrupted>(controller).is_some();
    }
    assert!(saw_marker, "the silent peer was interrupted before it left");
    assert!(
        server.world().resource::<NetNode>().peers().is_empty(),
        "the silent peer timed out"
    );
    assert!(
        server.world().get_entity(avatar).is_ok(),
        "the game kept the avatar"
    );
    assert!(server.world().get::<NetInterrupted>(avatar).is_none());
    assert_eq!(marked(&mut server), 0, "no entity keeps the marker");
}

/// Row 9: an interrupted peer's entity that is handed to the server or to another peer loses the
/// marker on the next tick; what the peer still owns keeps it.
#[test]
fn interrupted_marker_leaves_an_entity_handed_to_another_owner() {
    let net = LoopbackNet::new();
    let (mut server, mut client, peer) = linked_pair(&net, 7782, TimeoutPolicy::default(), 25_000);
    let kept = server.world_mut().spawn(NetOwner(peer)).id();
    let to_peer = server.world_mut().spawn(NetOwner(peer)).id();
    let to_server = server.world_mut().spawn(NetOwner(peer)).id();
    let released = server.world_mut().spawn(NetOwner(peer)).id();
    // 4 s into the outage: interrupted (1.5 s), far from the 20 s timeout.
    for _ in 0..400 {
        server.update();
        client.update();
    }
    for e in [kept, to_peer, to_server, released] {
        assert!(server.world().get::<NetInterrupted>(e).is_some());
    }
    server
        .world_mut()
        .entity_mut(to_peer)
        .insert(NetOwner(PeerId(peer.0 + 1)));
    server
        .world_mut()
        .entity_mut(to_server)
        .insert(NetOwner(PeerId::SERVER));
    server.world_mut().entity_mut(released).remove::<NetOwner>();
    server.update();
    assert!(server.world().get::<NetInterrupted>(kept).is_some());
    for e in [to_peer, to_server, released] {
        assert!(server.world().get::<NetInterrupted>(e).is_none(), "{e:?}");
    }
    assert_eq!(server.world().resource::<NetNode>().peers(), [peer]);
}

#[derive(Component, Clone, Debug, PartialEq, Default, regia_net::Replicate)]
struct Health(u32);

/// Row 12: the default `NetPlugin` registers no replicated component, so a game keeps the v0.5.0
/// protocol hash and component indices (on the base the plugin added `NetInterrupted` first and
/// every game's hash changed). Opting in to the marker changes the hash.
#[test]
fn net_plugin_keeps_the_v050_protocol_hash_and_the_marker_is_opt_in() {
    use regia_net::replication::{ReplicationRegistry, register_replicated};
    use regia_net::runtime::{ensure_resources, protocol_hash};
    use regia_net::{ReplicateAppExt, ReplicateOptions};
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(NetPlugin {
        mode: NetMode::DedicatedServer,
    });
    app.replicate::<Health>(ReplicateOptions::default());
    // The v0.5.0 way: the runtime's resources and the game's own registrations, nothing else.
    let mut v050 = World::new();
    ensure_resources(&mut v050);
    register_replicated::<Health>(&mut v050, ReplicateOptions::default());
    {
        let registry = app.world().resource::<ReplicationRegistry>();
        assert_eq!(registry.components().len(), 1);
        assert_eq!(registry.find("Health"), Some(0));
        assert_eq!(registry.find("NetInterrupted"), None);
    }
    assert_eq!(protocol_hash(app.world()), protocol_hash(&v050));
    NetPlugin::replicate_interrupted_marker(&mut app);
    let registry = app.world().resource::<ReplicationRegistry>();
    assert_eq!(registry.find("Health"), Some(0), "opting in appends");
    assert_eq!(registry.find("NetInterrupted"), Some(1));
    assert_ne!(protocol_hash(app.world()), protocol_hash(&v050));
}

/// Row 10: a session link whose transport is up but which never sends `Hello` is refused at
/// `handshake_timeout_s` (10 s by default), not held for the 30 s start grace (base: 30 s, so
/// a few addresses could hold every `max_handshaking` slot three times longer).
#[test]
fn net_session_refuses_a_link_without_hello_at_the_handshake_timeout() {
    use regia_net::session::net_session::SessionWire;
    use regia_net::session::{HostOptions, NetSession};
    let net = LoopbackNet::new();
    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server
        .host_on(net.bind(addr(7783)).expect("bind"), HostOptions::default(), true)
        .expect("host");
    let mut half_open: NetClient<SessionWire, _> = NetClient::connect(
        net.bind(addr(8003)).expect("bind"),
        addr(7783),
        ClientConfig::default(),
        &(),
    )
    .expect("connect");
    let mut pending_since = None;
    let mut refused_at = None;
    for _ in 0..1_500 {
        server.advance_manual_clock(10);
        half_open.update(server.now_ms());
        server.update();
        let now = server.now_ms();
        let pending = server.server().expect("server").handshaking();
        if pending == 1 && pending_since.is_none() {
            pending_since = Some(now);
        }
        if pending == 0 && pending_since.is_some() && refused_at.is_none() {
            refused_at = Some(now);
        }
    }
    let since = pending_since.expect("the transport came up");
    let refused = refused_at.expect("the half-open link was refused within 15 s");
    let held = refused - since;
    assert!((10_000..=10_020).contains(&held), "held {held} ms");
}

/// Row 16: a `timeout_ms` the file names wins over legacy `timeout_s`, even at its default value
/// (base: any non-default `timeout_s` overrode an explicit `timeout_ms: 20000`); the seconds
/// still count in a file without `timeout_ms`.
#[test]
fn net_ron_explicit_timeout_ms_wins_over_legacy_seconds() {
    use regia_net::NetSettings;
    let resolved = |text: &str| {
        NetSettings::from_ron(text)
            .expect("loads")
            .session
            .timeout_policy()
            .timeout_ms
    };
    for text in [
        "(format_version: 3, session: (timeout_ms: 20000, timeout_s: 10.0))",
        "(format_version: 3, session: (timeout_s: 10.0, timeout_ms: 20000))",
        "NetSettings(format_version: 3, session: SessionSettings(timeout_s: 10.0, timeout_ms: 20000))",
    ] {
        assert_eq!(resolved(text), 20_000, "{text}");
    }
    assert_eq!(
        resolved("(format_version: 3, session: (timeout_ms: 40000, timeout_s: 10.0))"),
        40_000
    );
    // Legacy files without `timeout_ms` keep their seconds.
    assert_eq!(
        resolved("(format_version: 2, session: (timeout_s: 10.0))"),
        10_000
    );
    assert_eq!(
        resolved("(format_version: 2, session: (timeout_s: 20.0))"),
        20_000
    );
    assert_eq!(resolved("(format_version: 3)"), 20_000);
    // The writer keeps the file's milliseconds.
    let saved = NetSettings::from_ron("(format_version: 3, session: (timeout_ms: 20000, timeout_s: 10.0))")
        .expect("loads")
        .to_ron()
        .expect("writes");
    assert_eq!(resolved(&saved), 20_000, "{saved}");
}
