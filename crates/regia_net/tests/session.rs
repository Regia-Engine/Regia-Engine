// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 15: the session lifecycle on the loopback hub with simulated time.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use regia_net::model::{NetPlayerId, PeerId};
use regia_net::session::bans::{BanKind, BanStore};
use regia_net::session::conditioner::ShaperSettings;
use regia_net::session::hub::{SessionHub, SimInput};
use regia_net::session::lifecycle::{
    AuthRequestId, AuthResult, ClientSessionEvent, ClientSessionState, HostOptions, JoinDecision,
    JoinSnapshot, LinkId, PeerAddr, PeerState, ServerSessionEvent, SessionAuthenticator, SessionSettings,
};
use regia_net::session::manifest::{ContentManifest, ManifestAsset, ManifestMod};
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};
use regia_net::session::streams::StreamEvent;
use regia_net::session::{NetSession, net_session::ConnectOptions, net_session::SessionError};
use regia_net::socket::LoopbackNet;

/// A fresh temp directory per call (pid + counter), like `regia_test_utils::unique_temp_dir`
/// (not a dependency here: it pulls the renderer).
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn asset(id: &str, kind: &str, h: u64) -> ManifestAsset {
    ManifestAsset {
        id: id.into(),
        kind: kind.into(),
        content_hash: h,
    }
}

fn base_assets() -> Vec<ManifestAsset> {
    vec![
        asset("levels/arena.level.ron", "level", 11),
        asset("prefabs/crate.prefab.ron", "prefab", 12),
        asset("textures/rock.ktx2", "texture", 13),
    ]
}

fn manifest() -> ContentManifest {
    let base = base_assets();
    let dlc = vec![asset("levels/harbor.level.ron", "level", 21)];
    ContentManifest::build(
        "0.5.0",
        "1.2.0",
        1,
        0xabc,
        &[("base", &base), ("dlc_harbor", &dlc)],
    )
    .with_mods(vec![ManifestMod {
        id: "speedrun".into(),
        version: "1.0".into(),
        hash: 31,
    }])
}

fn hello(names: &[&str]) -> Hello {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: names.iter().map(|s| (*s).to_owned()).collect(),
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: None,
    }
}

fn opts() -> HostOptions {
    HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    }
}

fn hub_with(settings: SessionSettings, opts: HostOptions) -> SessionHub {
    let mut hub = SessionHub::new(settings, opts, manifest());
    hub.server.set_dev_build(false);
    hub
}

fn new_hub() -> SessionHub {
    hub_with(SessionSettings::default(), opts())
}

fn server_events(hub: &mut SessionHub) -> Vec<ServerSessionEvent> {
    std::iter::from_fn(|| hub.server.poll_event()).collect()
}

fn states(log: &[ClientSessionEvent]) -> Vec<&'static str> {
    log.iter()
        .filter_map(|e| match e {
            ClientSessionEvent::StateChanged { to, .. } => Some(to.as_str()),
            _ => None,
        })
        .collect()
}

fn refused_with(h: Hello) -> DisconnectReason {
    let mut hub = new_hub();
    let c = hub.add_client(h);
    assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
    hub.clients
        .client(c)
        .disconnect_reason()
        .cloned()
        .expect("reason")
}

fn join(hub: &mut SessionHub, h: Hello) -> usize {
    let c = hub.add_client(h);
    assert!(
        hub.run_until(600, |h| h.clients.client(c).in_game()),
        "{:?}",
        hub.clients.client(c).state()
    );
    // Let the client's InGame acknowledgement reach the server.
    hub.run(3);
    c
}

// 1. Happy path, also under 10 % loss.
#[test]
fn happy_path_and_under_loss() {
    for loss in [0.0, 10.0] {
        let mut hub = new_hub();
        let lossy = ShaperSettings {
            latency_ms: 20,
            loss_pct: loss,
            ..ShaperSettings::default()
        };
        hub.set_conditioner(lossy, lossy, 99);
        let c = join(&mut hub, hello(&["Ann"]));
        assert_eq!(
            states(&hub.clients.client(c).log),
            ["connecting", "handshaking", "loading", "joining", "in_game"],
            "loss {loss}"
        );
        let events = server_events(&mut hub);
        let joined: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ServerSessionEvent::PlayerJoined { peer, players } => Some((*peer, players.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(joined.len(), 1);
        assert_eq!(
            joined[0].1,
            vec![NetPlayerId {
                peer: joined[0].0,
                local: 0
            }]
        );
        assert_eq!(hub.server.peer_state(joined[0].0), Some(PeerState::InGame));
    }
}

struct Auth(AuthResult, Option<AuthResult>);
impl SessionAuthenticator for Auth {
    fn check(&mut self, _: AuthRequestId, _: &Hello, _: &PeerAddr) -> AuthResult {
        self.0.clone()
    }
    fn poll(&mut self, _: AuthRequestId) -> Option<AuthResult> {
        self.1.clone()
    }
}

// 2. Each refusal, in the order of § 4.
#[test]
fn each_refusal_and_check_order() {
    let mut h = hello(&["Ann"]);
    h.protocol_version = 2;
    assert_eq!(refused_with(h.clone()).code(), "version_mismatch");
    // A client failing two checks (version and password) gets the first.
    h.password = Some("wrong".into());
    assert_eq!(refused_with(h).code(), "version_mismatch");

    let mut h = hello(&["Ann"]);
    h.protocol_hash = 0xdef;
    let r = refused_with(h);
    assert_eq!(r.code(), "protocol_mismatch");
    assert!(
        r.detail().is_some_and(|d| d.contains("0x0000000000000def")),
        "{r}"
    );

    let mut h = hello(&["Ann"]);
    h.manifest = h.manifest.with_mods(Vec::new());
    assert_eq!(refused_with(h).code(), "content_mismatch");

    // Password.
    let mut hub = hub_with(
        SessionSettings::default(),
        HostOptions {
            password: "secret".into(),
            ..opts()
        },
    );
    let bad = hub.add_client(hello(&["Ann"]));
    let mut ok = hello(&["Bo"]);
    ok.password = Some("secret".into());
    let good = hub.add_client(ok);
    hub.run(60);
    assert_eq!(
        hub.clients
            .client(bad)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("bad_password")
    );
    assert!(hub.clients.client(good).in_game());

    // Authenticator: reject, and pending past the timeout.
    for (auth, expect) in [
        (Auth(AuthResult::Reject("no licence"), None), "auth_failed"),
        (Auth(AuthResult::Pending, None), "auth_failed"),
        (
            Auth(
                AuthResult::Pending,
                Some(AuthResult::Accept {
                    platform_id: None,
                    reserved: false,
                }),
            ),
            "in_game",
        ),
    ] {
        let mut hub = new_hub();
        hub.server.set_authenticator(Box::new(auth));
        let c = hub.add_client(hello(&["Ann"]));
        hub.run_secs(12.0);
        let got = match hub.clients.client(c).state() {
            ClientSessionState::Disconnected(Some(r)) => r.code(),
            s => s.as_str(),
        };
        assert_eq!(got, expect);
    }

    // Capacity: players, not peers; too many local players.
    let mut hub = hub_with(
        SessionSettings::default(),
        HostOptions {
            max_players: 2,
            ..opts()
        },
    );
    join(&mut hub, hello(&["A", "B"]));
    let full = hub.add_client(hello(&["C"]));
    let nine = hub.add_client(hello(&["1", "2", "3", "4", "5", "6", "7", "8", "9"]));
    hub.run(60);
    assert_eq!(
        hub.clients
            .client(full)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("server_full")
    );
    assert_eq!(
        hub.clients
            .client(nine)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("too_many_local_players")
    );
}

// 2c. Spec 72 § 14: an Android listen server serves at most
// `replication.android_listen_max_clients` players even when `max_players` is higher. The Linux
// rig cannot run as Android, so the test installs the cap the host path installs on Android
// (`NetSession` syncs it from `NetSettings` and clamps the host options there).
#[test]
fn android_listen_server_caps_players_at_android_listen_max_clients() {
    let many = HostOptions {
        max_players: 16,
        ..opts()
    };
    let mut capped = hub_with(SessionSettings::default(), many.clone());
    capped.server.set_android_listen_cap(Some(8));
    for i in 0..8 {
        join(&mut capped, hello(&[&format!("p{i}")]));
    }
    let ninth = capped.add_client(hello(&["ninth"]));
    assert!(
        capped.run_until(600, |h| h.clients.client(ninth).disconnect_reason().is_some()),
        "the ninth player was not refused"
    );
    assert_eq!(
        capped.clients.client(ninth).disconnect_reason(),
        Some(&DisconnectReason::ServerFull)
    );

    // Control: the same numbers without the cap keep joining (16 max_players admits a ninth).
    let mut uncapped = hub_with(SessionSettings::default(), many.clone());
    for i in 0..9 {
        join(&mut uncapped, hello(&[&format!("q{i}")]));
    }

    // The cap survives a hot-reloaded `max_players`, and the reported capacity stays clamped
    // (muse fix-review, fix-net3-platform findings 2 and 3): `set_options` cannot raise the
    // capacity above the cap, and `options()` reports the enforced value.
    capped.server.set_options(HostOptions {
        max_players: 16,
        ..many.clone()
    });
    assert_eq!(
        capped.server.options().max_players,
        8,
        "the reported capacity stays clamped to the listen cap"
    );
    let ninth_after_reload = capped.add_client(hello(&["reloaded"]));
    assert!(
        capped.run_until(600, |h| h
            .clients
            .client(ninth_after_reload)
            .disconnect_reason()
            .is_some()),
        "the ninth player was not refused after a hot-reloaded max_players"
    );
    assert_eq!(
        capped.clients.client(ninth_after_reload).disconnect_reason(),
        Some(&DisconnectReason::ServerFull)
    );

    // `Some(0)` reads as the default 8 (spec 72 § 14): eight join, the ninth is refused.
    let mut zero = hub_with(SessionSettings::default(), many.clone());
    zero.server.set_android_listen_cap(Some(0));
    for i in 0..8 {
        join(&mut zero, hello(&[&format!("z{i}")]));
    }
    let ninth_zero = zero.add_client(hello(&["zero"]));
    assert!(
        zero.run_until(600, |h| h
            .clients
            .client(ninth_zero)
            .disconnect_reason()
            .is_some()),
        "the ninth player was not refused with a 0 cap"
    );
    assert_eq!(
        zero.clients.client(ninth_zero).disconnect_reason(),
        Some(&DisconnectReason::ServerFull)
    );
}

// 2a. ADR-1950: a client whose oldest protocol this server speaks joins, whatever the server's
// newest; older than the server's oldest is refused.
#[test]
fn an_older_protocol_joins_a_newer_server() {
    let mut newer = manifest();
    newer.protocol_version = regia_net::NET_PROTOCOL_VERSION;
    let mut hub = SessionHub::new(SessionSettings::default(), opts(), newer);
    hub.server.set_dev_build(false);
    let mut h = hello(&["Ann"]);
    h.protocol_version = regia_net::MIN_NET_PROTOCOL_VERSION;
    join(&mut hub, h);
    let mut h = hello(&["Bob"]);
    h.protocol_version = regia_net::MIN_NET_PROTOCOL_VERSION - 1;
    assert_eq!(refused_with_hub(&mut hub, h).code(), "version_mismatch");
}

// 2b. A Hello from a future protocol version whose body does not decode still gets
// `version_mismatch`.
#[test]
fn future_hello_body_still_gets_version_mismatch() {
    let mut hub = new_hub();
    hub.server
        .link_up(LinkId(5), SocketAddr::from(([10, 0, 0, 1], 1)), 0);
    let mut bytes = b"RGNH".to_vec();
    bytes.extend_from_slice(&7u32.to_le_bytes());
    bytes.extend_from_slice(&0xabcu64.to_le_bytes());
    bytes.extend_from_slice(&[0xee; 40]);
    hub.server.receive(LinkId(5), &bytes, 0);
    let out = hub.server.take_outgoing();
    assert_eq!(out.len(), 1);
    match SessionMsg::decode(&out[0].bytes).expect("decode") {
        SessionMsg::Refused(r) => assert_eq!(r.code(), "version_mismatch"),
        other => panic!("{other:?}"),
    }
}

// 3. Content mismatch detail.
#[test]
fn content_mismatch_lists_exactly_the_differing_items() {
    let base = base_assets();
    let client =
        ContentManifest::build("0.5.0", "1.2.0", 1, 0xabc, &[("base", &base)]).with_mods(vec![ManifestMod {
            id: "speedrun".into(),
            version: "1.1".into(),
            hash: 32,
        }]);
    let mut h = hello(&["Ann"]);
    h.manifest = client;
    let r = refused_with(h);
    assert_eq!(r.code(), "content_mismatch");
    let detail = r.detail().expect("detail");
    let lines: Vec<&str> = detail.lines().collect();
    assert_eq!(
        lines,
        [
            // The mod line names both versions (ADR-7259).
            "mod:speedrun: different (client 1.1, server 1.0)",
            "pack:dlc_harbor: missing on client"
        ]
    );

    // A different texture pak alone joins: textures are not hashed.
    let mut other = base_assets();
    other[2] = asset("textures/rock_4k.ktx2", "texture", 99);
    let dlc = vec![asset("levels/harbor.level.ron", "level", 21)];
    let mut h = hello(&["Ann"]);
    h.manifest = ContentManifest::build(
        "0.5.0",
        "1.2.0",
        1,
        0xabc,
        &[("base", &other), ("dlc_harbor", &dlc)],
    )
    .with_mods(manifest().mods);
    let mut hub = new_hub();
    join(&mut hub, h);
}

// 4. Timeouts per state; keepalives keep an idle connection for ten minutes.
#[test]
fn timeouts_per_state_and_keepalives() {
    // Connecting: the transport never comes up within 10 s.
    let mut hub = new_hub();
    hub.set_conditioner(
        ShaperSettings {
            latency_ms: 30_000,
            ..ShaperSettings::default()
        },
        ShaperSettings::default(),
        1,
    );
    let c = hub.add_client(hello(&["Ann"]));
    hub.run_secs(9.5);
    assert_eq!(*hub.clients.client(c).state(), ClientSessionState::Connecting);
    hub.run_secs(1.0);
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::Timeout)
    );

    // Handshaking: the Hello is lost for good; the server drops the link after 10 s, the client
    // after 10 s plus the authenticator allowance (10 s).
    let mut hub = new_hub();
    let c = hub.add_client(hello(&["Ann"]));
    hub.run_until(60, |h| {
        *h.clients.client(c).state() == ClientSessionState::Handshaking
    });
    hub.clients.cut(c);
    hub.run_secs(9.5);
    assert_eq!(hub.server.handshaking(), 1);
    hub.run_secs(1.0);
    assert_eq!(hub.server.handshaking(), 0);
    hub.run_secs(9.0);
    assert_eq!(*hub.clients.client(c).state(), ClientSessionState::Handshaking);
    hub.run_secs(1.0);
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::Timeout)
    );

    // Loading: a level that never loads → level_load_failed on both sides.
    let mut hub = new_hub();
    let c = hub.add_client(hello(&["Ann"]));
    hub.clients.client_mut(c).load_ms = None;
    hub.run_secs(119.0);
    assert_eq!(*hub.clients.client(c).state(), ClientSessionState::Loading);
    hub.run_secs(1.5);
    assert_eq!(
        hub.clients
            .client(c)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("level_load_failed")
    );
    let left = server_events(&mut hub).into_iter().find_map(|e| match e {
        ServerSessionEvent::PlayerLeft { reason, .. } => Some(reason.code()),
        _ => None,
    });
    assert_eq!(left, Some("level_load_failed"));

    // InGame silence: the server times the peer out (and with grace 0 retires it).
    let mut hub = hub_with(
        SessionSettings {
            timeout_ms: 10_000, // Explicit original deadline; the default is now 20 s.
            reconnect_grace_s: 0.0,
            ..SessionSettings::default()
        },
        opts(),
    );
    let c = join(&mut hub, hello(&["Ann"]));
    hub.clients.cut(c);
    hub.run_secs(9.5);
    assert_eq!(hub.server.peer_ids().len(), 1);
    hub.run_secs(1.0);
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::Timeout)
    );
    assert!(hub.server.peer_ids().is_empty());

    // Keepalives: ten idle minutes stay connected.
    let mut hub = new_hub();
    let c = join(&mut hub, hello(&["Ann"]));
    let start = hub.now_ms;
    hub.run_secs(600.0);
    assert!(hub.now_ms - start >= 600_000);
    assert!(hub.clients.client(c).in_game());
}

// 4b. Joining: the InGame marker never comes (the server's link is cut after LevelReady).
#[test]
fn joining_timeout() {
    let mut hub = new_hub();
    let c = hub.add_client(hello(&["Ann"]));
    assert!(hub.run_until(600, |h| *h.clients.client(c).state()
        == ClientSessionState::Joining));
    hub.clients.cut(c);
    hub.run_secs(29.5);
    assert_eq!(*hub.clients.client(c).state(), ClientSessionState::Joining);
    hub.run_secs(1.0);
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::Timeout)
    );
}

// 5. Join snapshot order: removed level entities before the first InGame frame.
#[test]
fn join_snapshot_order() {
    let mut hub = new_hub();
    hub.server.set_join_source(Box::new(|peer| JoinSnapshot {
        removed: vec![0xdead_beef],
        records: vec![vec![1, 2, 3], peer.0.to_le_bytes().to_vec()],
        tick: 42,
    }));
    let c = join(&mut hub, hello(&["Ann"]));
    let log = &hub.clients.client(c).log;
    let pos = |pred: &dyn Fn(&ClientSessionEvent) -> bool| log.iter().position(pred).expect("event");
    let removed = pos(&|e| matches!(e, ClientSessionEvent::JoinRemoved(g) if g == &vec![0xdead_beef]));
    let records = pos(&|e| matches!(e, ClientSessionEvent::JoinRecords(b) if b == &vec![1, 2, 3]));
    let in_game = pos(&|e| {
        matches!(
            e,
            ClientSessionEvent::StateChanged {
                to: ClientSessionState::InGame,
                ..
            }
        )
    });
    assert!(removed < records && records < in_game, "{log:?}");
}

// rc1-rev-net-k 4: a join snapshot record the session cannot frame (over its 60 KiB payload) is not
// dropped with the client sent `InGame` anyway, missing that state for good: the join is refused.
#[test]
fn a_join_record_too_large_to_send_refuses_the_join() {
    let mut hub = new_hub();
    hub.server.set_join_source(Box::new(|_| JoinSnapshot {
        removed: Vec::new(),
        records: vec![vec![1, 2, 3], vec![7; 61 * 1024]],
        tick: 42,
    }));
    let c = hub.add_client(hello(&["Ann"]));
    hub.run(600);
    let client = hub.clients.client(c);
    assert!(!client.in_game(), "{:?}", client.state());
    assert!(
        !client.log.iter().any(|e| matches!(
            e,
            ClientSessionEvent::StateChanged {
                to: ClientSessionState::InGame,
                ..
            }
        )),
        "never in game: {:?}",
        client.log
    );
    assert!(
        matches!(
            client.disconnect_reason(),
            Some(DisconnectReason::LevelLoadFailed(text)) if text.contains("join")
        ),
        "{:?}",
        client.disconnect_reason()
    );
}

// 6. Travel.
#[test]
fn travel_slow_and_fast_clients_stale_ready_and_restart() {
    let mut hub = new_hub();
    let fast = join(&mut hub, hello(&["Fast"]));
    let slow = join(&mut hub, hello(&["Slow"]));
    let fast_peer = hub.clients.client(fast).core.peer_id().expect("peer");
    let slow_peer = hub.clients.client(slow).core.peer_id().expect("peer");
    hub.clients.client_mut(slow).load_ms = Some(3000);
    let first = hub.server.travel("levels/harbor.level.ron");
    // The fast client is back in game while the slow one still loads.
    assert!(hub.run_until(120, |h| h.clients.client(fast).in_game()
        && h.clients.client(fast).core.travel_id() == first));
    assert_eq!(*hub.clients.client(slow).state(), ClientSessionState::Loading);
    assert_eq!(hub.server.peer_state(slow_peer), Some(PeerState::Loading));
    assert!(hub.run_until(300, |h| h.clients.client(slow).in_game()));
    // Same peers and players after travel.
    assert_eq!(hub.clients.client(fast).core.peer_id(), Some(fast_peer));
    assert_eq!(hub.clients.client(slow).core.peer_id(), Some(slow_peer));
    assert!(hub.clients.client(fast).log.iter().any(
        |e| matches!(e, ClientSessionEvent::TravelFinished { level } if level == "levels/harbor.level.ron")
    ));

    // A stale LevelReady is ignored.
    hub.clients.client_mut(slow).load_ms = None;
    let second = hub.server.travel("levels/arena.level.ron");
    hub.run(5);
    let link = hub.server.link_of(slow_peer).expect("link");
    let stale = SessionMsg::LevelReady { travel_id: first }
        .encode()
        .expect("encode");
    hub.server.receive(link, &stale, hub.now_ms);
    assert_eq!(hub.server.peer_state(slow_peer), Some(PeerState::Loading));

    // A travel during loading restarts loading with the new level.
    hub.clients.client_mut(slow).load_ms = Some(500);
    let third = hub.server.travel("levels/harbor.level.ron");
    assert_ne!(second, third);
    assert!(hub.run_until(300, |h| h.clients.client(slow).in_game()));
    assert_eq!(hub.clients.client(slow).core.travel_id(), third);
    assert_eq!(hub.clients.client(slow).core.level(), "levels/harbor.level.ron");
}

// 7. Every disconnect reason is produced and delivered (session-produced ones end to end; the
// transport and replication ones through the mapping, see `net_session` unit tests).
#[test]
fn disconnect_reasons_reach_both_sides() {
    let mut seen = BTreeSet::new();
    // client_quit: the server sees it; the client ends with it.
    let mut hub = new_hub();
    let c = join(&mut hub, hello(&["Ann"]));
    hub.clients
        .client_mut(c)
        .core
        .disconnect(DisconnectReason::ClientQuit, hub.now_ms);
    hub.run(10);
    let r = hub.clients.client(c).disconnect_reason().cloned().expect("ended");
    seen.insert(r.code());
    let left = server_events(&mut hub).into_iter().find_map(|e| match e {
        ServerSessionEvent::PlayerLeft { reason, .. } => Some(reason),
        _ => None,
    });
    assert_eq!(left, Some(DisconnectReason::ClientQuit));
    // kicked, server_shutting_down, malformed_packet.
    let mut hub = new_hub();
    let a = join(&mut hub, hello(&["A"]));
    let b = join(&mut hub, hello(&["B"]));
    let c = join(&mut hub, hello(&["C"]));
    let pa = hub.clients.client(a).core.peer_id().expect("peer");
    assert!(hub.server.kick(NetPlayerId { peer: pa, local: 0 }, "afk"));
    let pc = hub.clients.client(c).core.peer_id().expect("peer");
    let link_c = hub.server.link_of(pc).expect("link");
    hub.server.receive(link_c, &[0xfe, 0xff], hub.now_ms);
    hub.run(10);
    assert_eq!(
        hub.clients.client(a).disconnect_reason(),
        Some(&DisconnectReason::Kicked("afk".into()))
    );
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::MalformedPacket)
    );
    hub.server.drain();
    hub.run(10);
    assert_eq!(
        hub.clients.client(b).disconnect_reason(),
        Some(&DisconnectReason::ServerShuttingDown)
    );
    // Draining refuses new joins too.
    let late = hub.add_client(hello(&["Late"]));
    hub.run(30);
    assert_eq!(
        hub.clients.client(late).disconnect_reason(),
        Some(&DisconnectReason::ServerShuttingDown)
    );
    for code in ["kicked", "malformed_packet", "server_shutting_down"] {
        seen.insert(code);
    }
    // invalid_message: a name longer than 32 characters, or no player at all.
    let long = "x".repeat(33);
    assert_eq!(refused_with(hello(&[long.as_str()])).code(), "invalid_message");
    assert_eq!(refused_with(hello(&[])).code(), "invalid_message");
    seen.insert("invalid_message");
    // banned.
    let mut hub = new_hub();
    hub.server
        .bans_mut()
        .add(BanKind::Name, "Mallory", None, "griefing", 0)
        .expect("ban");
    let m = hub.add_client(hello(&["Mallory"]));
    hub.run(30);
    let r = hub
        .clients
        .client(m)
        .disconnect_reason()
        .cloned()
        .expect("refused");
    assert_eq!(r.code(), "banned");
    assert!(
        r.detail()
            .is_some_and(|d| d.contains("griefing") && d.contains("permanent"))
    );
    seen.insert("banned");
    // suspended: enters grace (checked in the reconnect test); transport_error: the server
    // closes a link without a goodbye.
    let mut hub = new_hub();
    let s = join(&mut hub, hello(&["S"]));
    hub.clients
        .client_mut(s)
        .core
        .disconnect(DisconnectReason::Suspended, hub.now_ms);
    hub.run(10);
    assert!(server_events(&mut hub).iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerDropped {
            reason: DisconnectReason::Suspended,
            ..
        }
    )));
    seen.insert("suspended");
    let t = join(&mut hub, hello(&["T"]));
    let pt = hub.clients.client(t).core.peer_id().expect("peer");
    let link_t = hub.server.link_of(pt).expect("link");
    hub.server.link_down(
        link_t,
        DisconnectReason::TransportError("reset".into()),
        hub.now_ms,
    );
    hub.run(10);
    assert!(server_events(&mut hub).iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerDropped {
            reason: DisconnectReason::TransportError(_),
            ..
        }
    )));
    seen.insert("transport_error");
    // The rest come from the refusal and timeout tests above.
    for code in [
        "timeout",
        "server_full",
        "too_many_local_players",
        "version_mismatch",
        "protocol_mismatch",
        "content_mismatch",
        "bad_password",
        "auth_failed",
        "level_load_failed",
        "flood",
        "reliable_overflow",
    ] {
        seen.insert(code);
    }
    let all: BTreeSet<&str> = regia_net::session::proto::DISCONNECT_CODES
        .iter()
        .copied()
        .collect();
    assert_eq!(seen, all);
}

// 8. Bans: each kind, expiry, atomic save, unreadable file.
#[test]
fn bans_kinds_expiry_and_files() {
    struct Pid;
    impl SessionAuthenticator for Pid {
        fn check(&mut self, _: AuthRequestId, h: &Hello, _: &PeerAddr) -> AuthResult {
            AuthResult::Accept {
                platform_id: Some(format!("steam:{}", h.player_names[0])),
                reserved: false,
            }
        }
    }
    let mut hub = new_hub();
    hub.server.set_authenticator(Box::new(Pid));
    let bans = hub.server.bans_mut();
    bans.add(BanKind::PlatformId, "steam:Eve", None, "cheating", 0)
        .expect("ban");
    bans.add(BanKind::Address, "10.9.0.0/16", None, "spam", 0)
        .expect("ban");
    bans.add(BanKind::Name, "Trent", None, "name", 0).expect("ban");
    bans.add(BanKind::Name, "Old", Some(1), "expired", 0)
        .expect("ban");
    let eve = hub.add_client(hello(&["Eve"]));
    let addr = hub.add_client(hello(&["Ann"]));
    hub.clients
        .set_addr(addr, SocketAddr::from(([10, 9, 4, 4], 5000)));
    let trent = hub.add_client(hello(&["Trent"]));
    let old = hub.add_client(hello(&["Old"]));
    hub.run(60);
    for c in [eve, addr, trent] {
        assert_eq!(
            hub.clients
                .client(c)
                .disconnect_reason()
                .map(DisconnectReason::code),
            Some("banned"),
            "client {c}"
        );
    }
    assert!(
        hub.clients.client(old).in_game(),
        "an expired entry does not match"
    );

    // Atomic save: a crash between write and rename leaves the old file intact.
    let dir = unique_temp_dir("session-bans");
    let path = dir.join("bans.ron");
    let mut store = BanStore::open(&path);
    store.add(BanKind::Name, "First", None, "r", 0).expect("save");
    let before = std::fs::read_to_string(&path).expect("written");
    store.add(BanKind::Name, "Second", None, "r", 0).expect("save");
    let mut crashing = BanStore::open(&path);
    crashing
        .add(BanKind::Name, "Third", None, "r", 0)
        .expect("in memory");
    let after_two = std::fs::read_to_string(&path).expect("file");
    assert!(after_two.contains("Second") && after_two.contains("Third"));
    // Simulated crash: write the temp file only.
    let mut list_only = BanStore::open(&path);
    let _ = list_only.list();
    let tmp = list_only.write_temp(0).expect("temp");
    std::fs::write(&tmp, "garbage half-written").expect("tmp");
    let reopened = BanStore::open(&path);
    assert!(reopened.writable());
    assert_eq!(reopened.list().entries.len(), 3);
    assert_ne!(before, after_two);

    // An unreadable file is not overwritten.
    let bad = dir.join("bad.ron");
    std::fs::write(&bad, "this is not ron (").expect("write");
    let mut store = BanStore::open(&bad);
    assert!(!store.writable());
    let _ = store.add(BanKind::Name, "X", None, "r", 0);
    assert_eq!(std::fs::read_to_string(&bad).expect("read"), "this is not ron (");
    let _ = std::fs::remove_dir_all(&dir);
}

// 9. Reconnect within the grace.
/// Audit M7 (muse): stream buffers have a limit across peers. The peer that goes past it loses its
/// streams and stays connected.
#[test]
fn stream_buffers_are_capped_across_peers() {
    let mut hub = hub_with(
        SessionSettings {
            max_stream_buffer_bytes: 1024,
            ..SessionSettings::default()
        },
        opts(),
    );
    let a = join(&mut hub, hello(&["A"]));
    let pa = hub.clients.client(a).core.peer_id().expect("peer");
    hub.clients
        .client_mut(a)
        .core
        .send_stream("map", vec![7; 64 * 1024])
        .expect("queued");
    hub.run(300);
    let events = server_events(&mut hub);
    let failed = events.iter().any(|e| {
        matches!(e, ServerSessionEvent::Stream {
            event: StreamEvent::Failed { reason, .. },
            ..
        } if reason == "stream_buffer_full")
    });
    assert!(failed, "the server gave up on the stream: {events:?}");
    assert_eq!(
        hub.server.peer_state(pa),
        Some(PeerState::InGame),
        "and kept the peer"
    );
}

/// Audit M3 (muse): a reconnect token works once. A resume hands out a new one, so a token sniffed
/// from an earlier `Welcome` cannot take the slot over at the next drop.
#[test]
fn a_reconnect_token_is_rotated_on_resume() {
    let mut hub = hub_with(
        SessionSettings {
            timeout_ms: 10_000, // Explicit original deadline; the default is now 20 s.
            reconnect_grace_s: 30.0,
            ..SessionSettings::default()
        },
        HostOptions {
            max_players: 4,
            ..opts()
        },
    );
    let a = join(&mut hub, hello(&["A"]));
    let pa = hub.clients.client(a).core.peer_id().expect("peer");
    let first = hub.clients.client(a).core.reconnect_token().expect("token");
    hub.clients.cut(a);
    hub.run_secs(10.5);
    let now = hub.now_ms;
    hub.clients.client_mut(a).reconnect(now, 1);
    assert!(hub.run_until(300, |h| h.clients.client(a).in_game()));
    assert_eq!(hub.clients.client(a).core.peer_id(), Some(pa));
    let second = hub.clients.client(a).core.reconnect_token().expect("token");
    assert_ne!(first, second, "the resume minted a new token");
    hub.run(10);

    hub.clients.cut(a);
    hub.run_secs(10.5);
    assert_eq!(hub.server.peer_state(pa), Some(PeerState::Grace));
    let mut stolen = hello(&["Thief"]);
    stolen.reconnect_token = Some(first);
    let thief = join(&mut hub, stolen);
    assert_ne!(
        hub.clients.client(thief).core.peer_id(),
        Some(pa),
        "the old token is dead"
    );
    assert_eq!(
        hub.server.peer_state(pa),
        Some(PeerState::Grace),
        "the slot is still waiting"
    );
    let now = hub.now_ms;
    hub.clients.client_mut(a).reconnect(now, 1);
    assert!(hub.run_until(300, |h| h.clients.client(a).in_game()));
    assert_eq!(
        hub.clients.client(a).core.peer_id(),
        Some(pa),
        "the real owner resumes"
    );
}

#[test]
fn reconnect_grace() {
    let mut hub = hub_with(
        SessionSettings {
            timeout_ms: 10_000, // Explicit original deadline; the default is now 20 s.
            reconnect_grace_s: 30.0,
            ..SessionSettings::default()
        },
        HostOptions {
            max_players: 2,
            ..opts()
        },
    );
    let a = join(&mut hub, hello(&["A"]));
    let b = join(&mut hub, hello(&["B"]));
    let pa = hub.clients.client(a).core.peer_id().expect("peer");
    // A drops silently; the server times it out into Grace; the server stays full.
    hub.clients.cut(a);
    hub.run_secs(10.5);
    assert_eq!(hub.server.peer_state(pa), Some(PeerState::Grace));
    let c = hub.add_client(hello(&["C"]));
    hub.run(30);
    assert_eq!(
        hub.clients
            .client(c)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("server_full")
    );
    // A comes back with its token: same PeerId, even though every slot is taken.
    let now = hub.now_ms;
    hub.clients.client_mut(a).reconnect(now, 1);
    assert!(hub.run_until(300, |h| h.clients.client(a).in_game()));
    assert_eq!(hub.clients.client(a).core.peer_id(), Some(pa));
    assert!(
        server_events(&mut hub)
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerResumed { peer } if *peer == pa))
    );
    // A quit never enters Grace.
    let pb = hub.clients.client(b).core.peer_id().expect("peer");
    hub.clients
        .client_mut(b)
        .core
        .disconnect(DisconnectReason::ClientQuit, hub.now_ms);
    hub.run(10);
    assert_eq!(hub.server.peer_state(pb), None);
    // After the grace a return is a new join with a new PeerId.
    hub.clients.cut(a);
    hub.run_secs(10.5 + 29.0);
    assert_eq!(hub.server.peer_state(pa), Some(PeerState::Grace));
    hub.run_secs(1.5);
    assert_eq!(hub.server.peer_state(pa), None);
    let now = hub.now_ms;
    hub.clients.client_mut(a).reconnect(now, 1);
    assert!(hub.run_until(300, |h| h.clients.client(a).in_game()));
    assert_ne!(hub.clients.client(a).core.peer_id(), Some(pa));

    // A suspended client resumes; a ban added during the grace refuses the resume.
    let mut hub = new_hub();
    let s = join(&mut hub, hello(&["Sus"]));
    let ps = hub.clients.client(s).core.peer_id().expect("peer");
    hub.clients
        .client_mut(s)
        .core
        .disconnect(DisconnectReason::Suspended, hub.now_ms);
    hub.run(10);
    assert_eq!(hub.server.peer_state(ps), Some(PeerState::Grace));
    let now = hub.now_ms;
    hub.clients.client_mut(s).reconnect(now, 1);
    assert!(hub.run_until(300, |h| h.clients.client(s).in_game()));
    assert_eq!(hub.clients.client(s).core.peer_id(), Some(ps));
    hub.clients
        .client_mut(s)
        .core
        .disconnect(DisconnectReason::Suspended, hub.now_ms);
    hub.run(10);
    hub.server
        .bans_mut()
        .add(BanKind::Name, "Sus", None, "late ban", 0)
        .expect("ban");
    let now = hub.now_ms;
    hub.clients.client_mut(s).reconnect(now, 1);
    hub.run(30);
    assert_eq!(
        hub.clients
            .client(s)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("banned")
    );
}

// 10. Streams.
#[test]
fn streams_paced_fail_on_drop_and_size_limit() {
    let rate = 8 * 1024 * 1024;
    let mut hub = hub_with(
        SessionSettings::default(),
        HostOptions {
            client_rate_bytes: rate,
            ..opts()
        },
    );
    let mut h = hello(&["Ann"]);
    h.client_rate_bytes = rate;
    let c = join(&mut hub, h);
    let peer = hub.clients.client(c).core.peer_id().expect("peer");
    let data: Vec<u8> = (0..10 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    hub.server.send_stream(peer, "map", data.clone()).expect("stream");
    let budget = hub.server.budget_bytes_per_tick(peer);
    let mut last = 0;
    let mut games = 0;
    for _ in 0..2000 {
        hub.server.send_game(peer, vec![1, 2, 3]);
        hub.step();
        let sent = hub.server.stream_bytes_sent(peer);
        assert!(
            (sent - last) as f64 <= budget * 0.25 + 1.0,
            "tick sent {} > 25 % of {budget}",
            sent - last
        );
        last = sent;
        games += 1;
        if hub
            .clients
            .client(c)
            .log
            .iter()
            .any(|e| matches!(e, ClientSessionEvent::Stream(StreamEvent::Complete { .. })))
        {
            break;
        }
    }
    let got = hub.clients.client(c).log.iter().find_map(|e| match e {
        ClientSessionEvent::Stream(StreamEvent::Complete { kind, bytes, .. }) => {
            Some((kind.clone(), bytes.len()))
        }
        _ => None,
    });
    assert_eq!(got, Some(("map".to_owned(), data.len())));
    let game_msgs = hub
        .clients
        .client(c)
        .log
        .iter()
        .filter(|e| matches!(e, ClientSessionEvent::Game(_)))
        .count();
    assert!(
        game_msgs + 5 >= games,
        "gameplay kept flowing: {game_msgs} of {games}"
    );
    assert!(
        hub.clients
            .client(c)
            .log
            .iter()
            .any(|e| matches!(e, ClientSessionEvent::Stream(StreamEvent::Progress { .. })))
    );

    // A drop mid-stream fails it on the receiver.
    hub.server
        .send_stream(peer, "replay", vec![7; 4 * 1024 * 1024])
        .expect("stream");
    hub.run(3);
    let now = hub.now_ms;
    hub.clients
        .client_mut(c)
        .core
        .link_down(DisconnectReason::TransportError("cable".into()), now);
    let events: Vec<_> = std::iter::from_fn(|| hub.clients.client_mut(c).core.poll_event()).collect();
    assert!(
        events.iter().any(|e| matches!(e, ClientSessionEvent::Stream(StreamEvent::Failed { reason, .. }) if reason == "transport_error")),
        "{events:?}"
    );

    // 65 MiB is refused.
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session.host_local(opts(), true).expect("host");
    session.add_simulated_clients(1, SimInput::Idle).expect("sim");
    for _ in 0..60 {
        session.advance_manual_clock(16);
        session.update();
    }
    let sim_peer = session.server().expect("server").peer_ids()[0];
    assert_eq!(
        session.send_stream(sim_peer, "big", vec![0; 65 * 1024 * 1024]),
        Err(SessionError::StreamTooLarge)
    );
}

// 11. The conditioner end to end: the same seed gives the same run; latency shows in ticks.
#[test]
fn conditioner_same_seed_same_run_and_latency_in_ticks() {
    let run = |seed: u64, latency: u32| {
        let mut hub = new_hub();
        let bad = ShaperSettings {
            latency_ms: latency,
            ..ShaperSettings::preset("bad").expect("preset")
        };
        hub.set_conditioner(bad, bad, seed);
        let c = hub.add_client(hello(&["Ann"]));
        let steps = (0..2000).take_while(|_| {
            hub.step();
            !hub.clients.client(c).in_game()
        });
        let n = steps.count();
        (n, format!("{:?}", hub.clients.client(c).log))
    };
    assert_eq!(run(5, 120), run(5, 120));
    let fast = run(5, 1).0;
    let slow = run(5, 300).0;
    // Connect + Hello + Welcome + LevelReady + InGame + ack: at least four one-way trips more.
    assert!(slow >= fast + 4 * 300 / 16, "fast {fast} slow {slow}");
}

// 12. Simulated clients.
#[test]
fn simulated_clients_join_play_and_leave() {
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session
        .host_local(
            HostOptions {
                max_players: 64,
                ..opts()
            },
            true,
        )
        .expect("host");
    let frames = vec![vec![1u8], vec![2u8], vec![3u8]];
    assert_eq!(
        session.add_simulated_clients(64, SimInput::Recording(frames)),
        Ok(64)
    );
    let mut games = 0usize;
    for _ in 0..120 {
        session.advance_manual_clock(16);
        session.update();
        games += session
            .take_server_events()
            .iter()
            .filter(|e| matches!(e, ServerSessionEvent::Game { .. }))
            .count();
    }
    let server = session.server().expect("server");
    let in_game = server
        .peer_ids()
        .into_iter()
        .filter(|p| server.peer_state(*p) == Some(PeerState::InGame))
        .count();
    assert_eq!(in_game, 64);
    assert!(games >= 64 * 50, "recorded inputs reach the server: {games}");
    // remove takes the newest first.
    let newest = *session.server().expect("server").peer_ids().last().expect("peer");
    session.remove_simulated_clients(1).expect("remove");
    for _ in 0..10 {
        session.advance_manual_clock(16);
        session.update();
    }
    assert_eq!(session.simulated_live(), 63);
    assert_eq!(session.peer_state(newest), None);
    assert_eq!(session.peer_state(PeerId(1)), Some(PeerState::InGame));
    session.clear_simulated_clients().expect("clear");
    for _ in 0..10 {
        session.advance_manual_clock(16);
        session.update();
    }
    assert_eq!(session.simulated_live(), 0);
    assert!(session.server().expect("server").peer_ids().is_empty());
    assert_eq!(
        session.add_simulated_clients(257, SimInput::Idle),
        Err(SessionError::SimClientsLimit)
    );
}

// § 9: the game's join hook runs after the engine's checks.
#[test]
fn join_rule_refuses_with_kicked() {
    let mut hub = new_hub();
    hub.server.set_join_rule(Box::new(|h: &Hello| {
        if h.player_names[0] == "Nope" {
            JoinDecision::Refuse("not on the list".into())
        } else {
            JoinDecision::Accept
        }
    }));
    let r = refused_with_hub(&mut hub, hello(&["Nope"]));
    assert_eq!(r, DisconnectReason::Kicked("not on the list".into()));
}

fn refused_with_hub(hub: &mut SessionHub, h: Hello) -> DisconnectReason {
    let c = hub.add_client(h);
    assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
    hub.clients
        .client(c)
        .disconnect_reason()
        .cloned()
        .expect("reason")
}

// Determinism (§ 13): two identical runs produce identical server event logs.
#[test]
fn lifecycle_replays_identically() {
    let run = || {
        let mut hub = new_hub();
        let lossy = ShaperSettings::preset("average").expect("preset");
        hub.set_conditioner(lossy, lossy, 3);
        for n in ["A", "B", "C"] {
            hub.add_client(hello(&[n]));
        }
        hub.run(120);
        hub.server.travel("levels/harbor.level.ron");
        hub.run(120);
        let ev = server_events(&mut hub);
        format!("{ev:?}")
    };
    assert_eq!(run(), run());
}

// Audit net1 (platform): the Android suspend/resume the app layer performs (spec 77 § 16).
#[test]
fn resume_after_suspend_sends_the_reconnect_token() {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = "127.0.0.1:7781".parse().expect("addr");
    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server
        .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
        .expect("host");
    let mut client = NetSession::default();
    client.set_manual_clock(0);
    client
        .connect_on(
            net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
            saddr,
            ConnectOptions::default(),
        )
        .expect("connect");
    let drive = |server: &mut NetSession, client: &mut NetSession| {
        for _ in 0..300 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
    };
    drive(&mut server, &mut client);
    assert_eq!(client.state(), ClientSessionState::InGame);
    let (peer, _token) = client
        .take_client_events()
        .iter()
        .find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some((w.peer_id, w.reconnect_token)),
            _ => None,
        })
        .expect("the welcome named the peer and carried a token");

    // The player presses Home: the goodbye leaves before frames stop, and the server holds the
    // grace slot instead of timing the peer out.
    client.suspend();
    server.advance_manual_clock(16);
    server.update();
    assert_eq!(server.peer_state(peer), Some(PeerState::Grace));

    // The process survived the pause: the resume connect carries the welcome token.
    assert!(
        client
            .resume_on(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"))
            .expect("resume"),
        "a reconnect began"
    );
    drive(&mut server, &mut client);
    assert_eq!(client.state(), ClientSessionState::InGame);
    let resumed_peer = client.take_client_events().iter().find_map(|e| match e {
        ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
        _ => None,
    });
    assert_eq!(resumed_peer, Some(peer), "the grace slot resumed the same PeerId");
    assert!(
        server
            .take_server_events()
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerResumed { peer: p } if *p == peer)),
        "the server reported PlayerResumed"
    );
}

/// ADR-8694: the resolved rule set's content hash joins the session manifest (the pack `@rules`), so a
/// client whose rules differ from the server's is refused with `content_mismatch` on the existing
/// handshake path, and a client with the same rules joins.
#[test]
fn peers_with_different_rulesets_fail_the_content_handshake() {
    use regia_rules::mode::MatchMode;
    use regia_rules::ruleset::{RuleSet, RuleSetLibrary};

    let mut library = RuleSetLibrary::new();
    library
        .insert(RuleSet::new("ctf", MatchMode::CaptureTheFlag))
        .expect("ctf");
    let mut long = RuleSet::new("ctf_long", MatchMode::CaptureTheFlag);
    long.session.objectives.capture_limit = Some(5);
    library.insert(long).expect("ctf_long");
    let none = std::collections::BTreeMap::new();
    let server_rules = library.resolve("ctf", &none).expect("ctf").content_hash();
    let client_rules = library
        .resolve("ctf_long", &none)
        .expect("ctf_long")
        .content_hash();
    assert_ne!(server_rules, client_rules, "one capture limit apart");

    let hub_with_rules = |rules: u64| {
        let mut hub = SessionHub::new(SessionSettings::default(), opts(), manifest().with_rules(rules));
        hub.server.set_dev_build(false);
        hub
    };
    for client in [Some(client_rules), None] {
        let mut hub = hub_with_rules(server_rules);
        let mut h = hello(&["Ann"]);
        h.manifest = match client {
            Some(rules) => manifest().with_rules(rules),
            None => manifest(),
        };
        let c = hub.add_client(h);
        assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
        let reason = hub
            .clients
            .client(c)
            .disconnect_reason()
            .cloned()
            .expect("reason");
        assert_eq!(reason.code(), "content_mismatch", "{client:?}");
        let detail = reason.detail().expect("detail");
        assert!(detail.contains("pack:@rules"), "{client:?}: {detail}");
        assert_eq!(detail.lines().count(), 1, "only the rules differ: {detail}");
    }

    // The same rules join.
    let mut hub = hub_with_rules(server_rules);
    let mut h = hello(&["Ann"]);
    h.manifest = manifest().with_rules(server_rules);
    join(&mut hub, h);

    // NetSession carries the hash into the manifest it presents at session start.
    let mut session = NetSession::default();
    assert!(
        !session
            .session_manifest()
            .packs
            .iter()
            .any(|(name, _)| name == "@rules")
    );
    session.set_rules_hash(Some(server_rules));
    assert_eq!(session.rules_hash(), Some(server_rules));
    assert!(
        session
            .session_manifest()
            .packs
            .contains(&("@rules".to_owned(), server_rules))
    );
}
