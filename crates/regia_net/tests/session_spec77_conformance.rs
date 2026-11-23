// SPDX-License-Identifier: MPL-2.0
//! Spec 77 conformance (lane sprint-spec-32-77-session-lifecycle): behavior the code already had
//! that no test proved. Each test names the spec section it covers. Loopback hub, simulated time.

use std::net::SocketAddr;

use regia_net::connection::Channel;
use regia_net::model::NetPlayerId;
use regia_net::session::bans::{BanKind, BanStore};
use regia_net::session::hub::SessionHub;
use regia_net::session::lifecycle::{
    AuthRequestId, AuthResult, ClientSessionEvent, ClientSessionState, HostOptions, LinkId, PeerAddr,
    PeerState, ServerSessionEvent, SessionAuthenticator, SessionClient, SessionServer, SessionSettings,
};
use regia_net::session::manifest::{ContentManifest, MAX_MANIFEST_DIFF_ITEMS, ManifestAsset, ManifestMod};
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};

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

fn manifest() -> ContentManifest {
    let base = vec![
        asset("levels/arena.level.ron", "level", 11),
        asset("prefabs/crate.prefab.ron", "prefab", 12),
    ];
    ContentManifest::build("0.5.0", "1.2.0", 1, 0xabc, &[("base", &base)])
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

/// Runs a client until it is in game (or fails the test) and lets its acknowledgement land.
fn join(hub: &mut SessionHub, h: Hello) -> usize {
    let c = hub.add_client(h);
    assert!(
        hub.run_until(600, |h| h.clients.client(c).in_game()),
        "{:?}",
        hub.clients.client(c).state()
    );
    hub.run(3);
    c
}

/// Runs a client until it ends and returns why.
fn refused(hub: &mut SessionHub, h: Hello) -> DisconnectReason {
    let c = hub.add_client(h);
    assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
    hub.clients
        .client(c)
        .disconnect_reason()
        .cloned()
        .expect("reason")
}

/// Marks every client whose first name starts with `VIP` as reserved.
struct VipAuth;
impl SessionAuthenticator for VipAuth {
    fn check(&mut self, _: AuthRequestId, h: &Hello, _: &PeerAddr) -> AuthResult {
        AuthResult::Accept {
            platform_id: None,
            reserved: h.player_names.first().is_some_and(|n| n.starts_with("VIP")),
        }
    }
}

/// § 5: reserved slots are only usable by clients the authenticator marks `reserved`; other
/// clients see `max_players - reserved_slots`.
#[test]
fn reserved_slots_admit_only_reserved_clients() {
    let mut hub = hub_with(
        SessionSettings {
            reserved_slots: 1,
            ..SessionSettings::default()
        },
        HostOptions {
            max_players: 2,
            ..opts()
        },
    );
    hub.server.set_authenticator(Box::new(VipAuth));
    join(&mut hub, hello(&["Ann"]));
    // The second ordinary client would take the reserved slot.
    assert_eq!(refused(&mut hub, hello(&["Bo"])).code(), "server_full");
    // A reserved client takes it.
    let vip = join(&mut hub, hello(&["VIP Cy"]));
    assert!(hub.clients.client(vip).in_game());
    assert_eq!(hub.server.players_in_use(), 2);
    // Past `max_players` even a reserved client is refused.
    assert_eq!(refused(&mut hub, hello(&["VIP Di"])).code(), "server_full");
}

/// § 5: an empty server password accepts any client password.
#[test]
fn an_empty_server_password_accepts_any_client_password() {
    let mut hub = new_hub();
    let mut h = hello(&["Ann"]);
    h.password = Some("anything at all".into());
    let c = join(&mut hub, h);
    assert!(hub.clients.client(c).in_game());
}

/// § 3.2: at most `session.max_handshaking` peers are in `Handshaking`; further transports are
/// closed at once.
#[test]
fn transports_past_max_handshaking_are_closed() {
    let settings = SessionSettings {
        max_handshaking: 4,
        ..SessionSettings::default()
    };
    let mut server = SessionServer::new(settings, opts(), manifest(), [7; 32]);
    for i in 0..5u32 {
        server.link_up(
            LinkId(i + 1),
            SocketAddr::from(([10, 0, 0, 1], 1000 + i as u16)),
            0,
        );
    }
    assert_eq!(server.handshaking(), 4, "the fifth link was not admitted");
    assert_eq!(
        server.take_closes(),
        vec![LinkId(5)],
        "and is closed without waiting"
    );
    // A free handshaking place admits the next transport again.
    server.link_down(LinkId(1), DisconnectReason::ClientQuit, 0);
    server.link_up(LinkId(6), SocketAddr::from(([10, 0, 0, 1], 2000)), 0);
    assert_eq!(server.handshaking(), 4);
    assert!(server.take_closes().is_empty());
}

/// § 4 item 1: `version_mismatch` carries both versions in its detail.
#[test]
fn version_mismatch_detail_names_both_versions() {
    let mut hub = new_hub();
    let mut h = hello(&["Ann"]);
    h.protocol_version = 2;
    let r = refused(&mut hub, h);
    assert_eq!(r.code(), "version_mismatch");
    let detail = r.detail().expect("detail");
    assert!(detail.contains('2') && detail.contains('1'), "{detail}");
}

/// § 4.1: a mismatch lists at most 32 differing items, sorted by name.
#[test]
fn content_mismatch_detail_stops_at_32_items() {
    let mut h = hello(&["Ann"]);
    let mods: Vec<ManifestMod> = (0..40)
        .map(|i| ManifestMod {
            id: format!("mod{i:02}"),
            version: "1".into(),
            hash: i,
        })
        .collect();
    h.manifest = h.manifest.with_mods(mods);
    let r = refused(&mut new_hub(), h);
    assert_eq!(r.code(), "content_mismatch");
    let lines: Vec<&str> = r.detail().expect("detail").lines().collect();
    assert_eq!(lines.len(), MAX_MANIFEST_DIFF_ITEMS);
    assert_eq!(lines[0], "mod:mod00: missing on server");
    assert_eq!(lines[31], "mod:mod31: missing on server");
}

/// § 4.1: `dev_allow_content_mismatch` downgrades a mismatch to a warning in dev builds only, and
/// never for another manifest `format_version` (detail `manifest_version`).
#[test]
fn dev_allow_content_mismatch_applies_to_dev_builds_and_not_to_manifest_versions() {
    let allow = SessionSettings {
        dev_allow_content_mismatch: true,
        ..SessionSettings::default()
    };
    let different = || {
        let mut h = hello(&["Ann"]);
        h.manifest = h.manifest.with_mods(vec![ManifestMod {
            id: "extra".into(),
            version: "1".into(),
            hash: 9,
        }]);
        h
    };
    // Dev build with the setting: joins.
    let mut dev = hub_with(allow.clone(), opts());
    dev.server.set_dev_build(true);
    join(&mut dev, different());
    // Release build with the same setting: refused.
    let mut release = hub_with(allow.clone(), opts());
    assert_eq!(refused(&mut release, different()).code(), "content_mismatch");
    // Dev build without the setting: refused.
    let mut strict = new_hub();
    strict.server.set_dev_build(true);
    assert_eq!(refused(&mut strict, different()).code(), "content_mismatch");
    // Another manifest version is refused even in a dev build with the setting.
    let mut dev = hub_with(allow, opts());
    dev.server.set_dev_build(true);
    let mut h = hello(&["Ann"]);
    h.manifest.format_version += 1;
    let r = refused(&mut dev, h);
    assert_eq!(r.code(), "content_mismatch");
    assert!(
        r.detail().is_some_and(|d| d.starts_with("manifest_version")),
        "{r}"
    );
}

/// § 7.2: kicking one player of a split-screen peer removes that player only; kicking its last
/// player disconnects the peer with `kicked`. Kicks never enter `Grace` (§ 7.4).
#[test]
fn kicking_one_split_screen_player_keeps_the_peer_until_its_last_player() {
    let mut hub = new_hub();
    let c = join(&mut hub, hello(&["Ann", "Bo"]));
    let peer = hub.clients.client(c).core.peer_id().expect("peer");
    let _ = server_events(&mut hub);
    assert!(hub.server.kick(NetPlayerId { peer, local: 1 }, "afk"));
    hub.run(5);
    assert!(hub.clients.client(c).in_game(), "the peer stays");
    assert_eq!(hub.server.peer_state(peer), Some(PeerState::InGame));
    assert_eq!(hub.server.players_in_use(), 1);
    assert!(hub.clients.client(c).log.iter().any(|e| matches!(
        e,
        ClientSessionEvent::PlayerRemoved { local: 1, reason } if *reason == DisconnectReason::Kicked("afk".into())
    )));
    let left: Vec<_> = server_events(&mut hub)
        .into_iter()
        .filter_map(|e| match e {
            ServerSessionEvent::PlayerLeft { player, reason } => Some((player, reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        left,
        vec![(
            NetPlayerId { peer, local: 1 },
            DisconnectReason::Kicked("afk".into())
        )]
    );
    // An unknown player is refused.
    assert!(!hub.server.kick(NetPlayerId { peer, local: 1 }, "again"));
    // The last player: the peer goes, straight to Gone.
    assert!(hub.server.kick(NetPlayerId { peer, local: 0 }, "bye"));
    hub.run(5);
    assert_eq!(
        hub.clients.client(c).disconnect_reason(),
        Some(&DisconnectReason::Kicked("bye".into()))
    );
    assert_eq!(hub.server.peer_state(peer), None, "no grace after a kick");
    assert!(
        !server_events(&mut hub)
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerDropped { .. }))
    );
}

/// § 3.1 `Disconnecting`: the client sends its goodbye three times, unreliably, one per tick, then
/// is `Disconnected(reason)`; it sends no game messages meanwhile.
#[test]
fn a_client_goodbye_is_three_unreliable_copies_then_disconnected() {
    let mut client = SessionClient::new(SessionSettings::default());
    assert!(client.connect(hello(&["Ann"]), 0));
    client.link_up(0);
    let _hello = client.take_outgoing();
    client.disconnect(DisconnectReason::ClientQuit, 0);
    assert_eq!(*client.state(), ClientSessionState::Disconnecting);
    let mut copies = 0;
    for tick in 1..=3u64 {
        client.send_game(vec![1, 2, 3]);
        client.update(tick * 16);
        for (channel, bytes) in client.take_outgoing() {
            match SessionMsg::decode(&bytes).expect("decode") {
                SessionMsg::Goodbye(r) => {
                    assert_eq!(r, DisconnectReason::ClientQuit);
                    assert_eq!(channel, Channel::Unreliable);
                    copies += 1;
                }
                other => panic!("tick {tick}: only goodbyes leave while disconnecting, got {other:?}"),
            }
        }
        assert_eq!(*client.state(), ClientSessionState::Disconnecting, "tick {tick}");
    }
    assert_eq!(copies, 3);
    client.update(4 * 16);
    assert!(client.take_outgoing().is_empty());
    assert_eq!(
        *client.state(),
        ClientSessionState::Disconnected(Some(DisconnectReason::ClientQuit))
    );
    assert!(client.take_close(), "the driver is told to close the transport");
}

/// § 7.4: when the grace expires the server reports `PlayerLeft { reason: timeout }` and the
/// peer is `Gone` (retired).
#[test]
fn grace_expiry_reports_player_left_with_timeout() {
    let mut hub = hub_with(
        SessionSettings {
            timeout_ms: 10_000, // Keep the original silence deadline explicit.
            reconnect_grace_s: 5.0,
            ..SessionSettings::default()
        },
        opts(),
    );
    let c = join(&mut hub, hello(&["Ann"]));
    let peer = hub.clients.client(c).core.peer_id().expect("peer");
    hub.clients.cut(c);
    hub.run_secs(10.5);
    assert_eq!(hub.server.peer_state(peer), Some(PeerState::Grace));
    let events = server_events(&mut hub);
    assert!(events.iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerDropped { peer: p, reason: DisconnectReason::Timeout } if *p == peer
    )));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerLeft { .. })),
        "no PlayerLeft while the slot waits"
    );
    hub.run_secs(5.5);
    assert_eq!(hub.server.peer_state(peer), None);
    let events = server_events(&mut hub);
    assert!(events.iter().any(|e| matches!(
        e,
        ServerSessionEvent::PeerStateChanged { peer: p, from: PeerState::Grace, to: PeerState::Gone } if *p == peer
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerLeft { player, reason: DisconnectReason::Timeout } if player.peer == peer
    )));
}

/// § 7.4: `reconnect_grace_s: 0` disables the grace: an unexpected drop retires the peer.
#[test]
fn a_zero_grace_retires_a_dropped_peer_at_once() {
    let mut hub = hub_with(
        SessionSettings {
            reconnect_grace_s: 0.0,
            ..SessionSettings::default()
        },
        opts(),
    );
    let c = join(&mut hub, hello(&["Ann"]));
    let peer = hub.clients.client(c).core.peer_id().expect("peer");
    let link = hub.server.link_of(peer).expect("link");
    let now = hub.now_ms;
    hub.server
        .link_down(link, DisconnectReason::TransportError("reset".into()), now);
    assert_eq!(hub.server.peer_state(peer), None);
    assert!(server_events(&mut hub).iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerLeft {
            reason: DisconnectReason::TransportError(_),
            ..
        }
    )));
}

/// § 6.2: a client that joins after a travel began receives the new level in `Welcome`, with the
/// current travel id, and its `LevelReady` for that travel is accepted.
#[test]
fn a_client_joining_during_travel_gets_the_new_level() {
    let mut hub = new_hub();
    let slow = join(&mut hub, hello(&["Slow"]));
    hub.clients.client_mut(slow).load_ms = None;
    let travel_id = hub.server.travel("levels/harbor.level.ron");
    hub.run(3);
    let late = join(&mut hub, hello(&["Late"]));
    let welcome = hub.clients.client(late).core.welcome().expect("welcomed").clone();
    assert_eq!(welcome.level, "levels/harbor.level.ron");
    assert_eq!(welcome.travel_id, travel_id);
    assert_eq!(hub.clients.client(late).core.level(), "levels/harbor.level.ron");
    // The slow client still loads; the late one did not wait for it.
    assert_eq!(*hub.clients.client(slow).state(), ClientSessionState::Loading);
}

/// § 7.3: expired entries are ignored and removed on the next save; the file is re-read on
/// reload, which picks up an edit made outside the process.
#[test]
fn expired_bans_leave_the_file_on_the_next_save_and_reload_reads_edits() {
    let dir = unique_temp_dir("spec77-bans");
    let path = dir.join("bans.ron");
    let mut store = BanStore::open(&path);
    store
        .add(BanKind::Name, "Brief", Some(100), "cool down", 0)
        .expect("save");
    store
        .add(BanKind::Name, "Forever", None, "cheating", 0)
        .expect("save");
    let text = std::fs::read_to_string(&path).expect("written");
    assert!(text.contains("Brief") && text.contains("Forever"), "{text}");
    // The next save after the expiry drops the expired entry from the file and the list.
    store
        .add(BanKind::Name, "Later", None, "spam", 200)
        .expect("save");
    let text = std::fs::read_to_string(&path).expect("written");
    assert!(!text.contains("Brief"), "the expired entry is gone: {text}");
    assert!(text.contains("Forever") && text.contains("Later"), "{text}");
    assert_eq!(store.list().entries.len(), 2);

    // An edit by another process (here: a second store on the same file) reaches the first on
    // reload.
    let mut other = BanStore::open(&path);
    other
        .add(BanKind::Name, "Edited", None, "by hand", 300)
        .expect("save");
    assert!(!store.list().entries.iter().any(|e| e.value == "Edited"));
    store.reload().expect("reload");
    assert!(store.list().entries.iter().any(|e| e.value == "Edited"));
    let _ = std::fs::remove_dir_all(&dir);
}
