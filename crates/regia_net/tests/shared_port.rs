// SPDX-License-Identifier: MPL-2.0
//! ADR-2665 (6): one game port for both layers. A replication game's link and the host's session
//! share one socket through `SharedPort`; a link client and a session client join the same
//! address. The first test runs on the in-memory `LoopbackNet` (no sockets, fixed steps); the
//! second runs the host with `--shared-port` on an OS-picked loopback port (`UdpAnyPort`), as the
//! existing host e2e tests do.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::model::NetMode;
use regia_net::server_host::config::CliOverrides;
use regia_net::server_host::{
    AppHostGame, HostTransport, HostedLink, PlayerLayer, PortLayer, ServerHost, SharedPort, TestClock,
    ban_link_peer, player_layer_in_world,
};
use regia_net::session::NetSession;
use regia_net::session::bans::BanStore;
use regia_net::session::lifecycle::{HostOptions, SessionSettings};
use regia_net::session::net_session::{ConnectOptions, ConnectTarget};
use regia_net::socket::LoopbackNet;
use regia_net::{CryptoMode, NetGame, NetPlugin, protocol_id};

struct Wire;
impl NetGame for Wire {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.shared_port";
    const PROTOCOL_VERSION: u32 = 1;
}

fn manual() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 16_000,
        },
        ..LinkOptions::default()
    }
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("address")
}

fn link_app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

#[test]
fn a_link_client_and_a_session_client_join_one_port() {
    let net = LoopbackNet::new();
    let server_addr = addr("10.3.0.1:7000");
    let socket = net.bind(server_addr).expect("bind");
    let (port, session_half, link_half) = SharedPort::split(Box::new(socket), protocol_id::<Wire>());
    assert_eq!(session_half.layer(), PortLayer::Session);

    // The game's link on one half.
    let mut server = link_app(NetMode::DedicatedServer);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<Wire>(Box::new(link_half), &manual())
        .expect("link hosts");
    // The host's session on the other.
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session
        .host_on(
            session_half,
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("session hosts");
    assert_eq!(session.local_addr(), Some(server_addr), "the same address");

    let mut link_client = link_app(NetMode::Client);
    link_client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<Wire>(
            Box::new(net.bind(addr("10.3.0.2:5000")).expect("bind")),
            server_addr,
            &manual(),
        )
        .expect("connect");
    let mut session_client = NetSession::new(SessionSettings::default(), session.manifest().clone());
    session_client.set_manual_clock(0);
    session_client
        .connect_on(
            net.bind(addr("10.3.0.3:5000")).expect("bind"),
            server_addr,
            ConnectOptions::default(),
        )
        .expect("connect");

    let mut joined = (false, false);
    for _ in 0..300 {
        server.update();
        link_client.update();
        session.advance_manual_clock(16);
        session.update();
        session_client.advance_manual_clock(16);
        session_client.update();
        joined = (
            link_client.world().resource::<NetLink>().is_connected()
                && server.world().resource::<NetLink>().peer_addrs().len() == 1,
            session_client.state().as_str() == "in_game",
        );
        if joined == (true, true) {
            break;
        }
    }
    assert_eq!(
        joined,
        (true, true),
        "link joined, session state {}",
        session_client.state().as_str()
    );
    assert_eq!(port.route_of(addr("10.3.0.2:5000")), Some(PortLayer::Link));
    assert_eq!(port.route_of(addr("10.3.0.3:5000")), Some(PortLayer::Session));
    let stats = port.stats();
    assert!(stats.routed[0] > 0 && stats.routed[1] > 0, "{stats:?}");
    assert_eq!(stats.routes, 2);
    assert_eq!(stats.dropped_full, 0);
    // The session counts its player, the link its peer: neither sees the other's client.
    assert_eq!(
        session.server().map(|s| s.peer_ids().len()),
        Some(1),
        "one session peer"
    );
}

/// A link client and a session client on one address (one NAT, two ports): the layers give them
/// the same id, so a bare id names nobody, and a ban of the link peer reaches both.
#[test]
fn an_id_on_both_layers_must_say_which_and_a_ban_takes_the_whole_address() {
    let dir = temp_dir("shared-ids");
    let net = LoopbackNet::new();
    let server_addr = addr("10.3.1.1:7000");
    let socket = net.bind(server_addr).expect("bind");
    let (_port, session_half, link_half) = SharedPort::split(Box::new(socket), protocol_id::<Wire>());
    let mut server = link_app(NetMode::DedicatedServer);
    server.insert_resource(HostedLink::new::<Wire>(manual()));
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<Wire>(Box::new(link_half), &manual())
        .expect("link hosts");
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session
        .host_on(
            session_half,
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("session hosts");
    *session.server_mut().expect("server").bans_mut() = BanStore::open(&dir.join("bans.ron"));
    let manifest = session.manifest().clone();
    server.insert_resource(session);

    let mut link_client = link_app(NetMode::Client);
    link_client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<Wire>(
            Box::new(net.bind(addr("10.3.1.2:5000")).expect("bind")),
            server_addr,
            &manual(),
        )
        .expect("connect");
    let mut session_client = NetSession::new(SessionSettings::default(), manifest);
    session_client.set_manual_clock(0);
    session_client
        .connect_on(
            net.bind(addr("10.3.1.2:5001")).expect("bind"),
            server_addr,
            ConnectOptions::default(),
        )
        .expect("connect");

    let step = |server: &mut App, link_client: &mut App, session_client: &mut NetSession| {
        server.update();
        link_client.update();
        let mut s = server.world_mut().resource_mut::<NetSession>();
        s.advance_manual_clock(16);
        s.update();
        session_client.advance_manual_clock(16);
        session_client.update();
    };
    for _ in 0..300 {
        step(&mut server, &mut link_client, &mut session_client);
        if link_client.world().resource::<NetLink>().is_connected()
            && session_client.state().as_str() == "in_game"
        {
            break;
        }
    }
    assert_eq!(session_client.state().as_str(), "in_game");
    let (link_peer, _) = server.world().resource::<NetLink>().peer_addrs()[0];
    let session_peers = server
        .world()
        .resource::<NetSession>()
        .server()
        .expect("server")
        .peer_ids();
    assert_eq!(
        session_peers,
        [link_peer],
        "the layers number their players on their own"
    );

    // A bare id names neither; a prefixed one names its layer.
    let bare = format!("{}.0", link_peer.0);
    assert_eq!(
        player_layer_in_world(server.world(), &bare).map_err(|e| e.code),
        Err("player_ambiguous")
    );
    let on_link = format!("link:{bare}");
    let on_session = format!("session:{bare}");
    assert_eq!(
        player_layer_in_world(server.world(), &on_link).map_err(|e| e.code),
        Ok((PlayerLayer::Link, bare.as_str()))
    );
    assert_eq!(
        player_layer_in_world(server.world(), &on_session).map_err(|e| e.code),
        Ok((PlayerLayer::Session, bare.as_str()))
    );

    // A ban of the link peer's address leaves no session peer on that address.
    ban_link_peer(server.world_mut(), &bare, None, "spam").expect("ban");
    for _ in 0..30 {
        step(&mut server, &mut link_client, &mut session_client);
    }
    assert!(server.world().resource::<NetLink>().peer_addrs().is_empty());
    assert_eq!(
        server
            .world()
            .resource::<NetSession>()
            .server()
            .expect("server")
            .peer_ids(),
        []
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-net-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

const SERVER_TOML: &str = r#"
format_version = 1
[server]
name = "Shared"
bind = "127.0.0.1"
port = 7790
max_players = 8
tick_hz = 60
[levels]
rotation = ["levels/a.level.ron"]
[net]
encryption = "required"
identity_file = "server.key"
[logs]
dir = "logs"
"#;

fn host(dir: &Path, cli: &[&str]) -> ServerHost {
    let cli = CliOverrides::parse(&cli.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).expect("cli");
    ServerHost::new(dir, cli, Box::new(|_| None), Box::new(TestClock::new())).expect("config")
}

fn link_game() -> AppHostGame {
    let mut app = link_app(NetMode::DedicatedServer);
    app.insert_resource(HostedLink::new::<Wire>(manual()));
    AppHostGame::new(app)
}

#[test]
fn the_host_serves_both_layers_on_one_port_with_the_switch() {
    // Off (the default): the game owns the port and the session runs local.
    let dir = temp_dir("shared-off");
    write(&dir.join("server.toml"), SERVER_TOML);
    let mut host_off = host(&dir, &[]);
    let mut game_off = link_game();
    host_off
        .start(&mut game_off, HostTransport::UdpAnyPort)
        .expect("start");
    assert!(host_off.game_owns_port());
    assert!(host_off.shared_port().is_none());
    assert_eq!(game_off.app.world().resource::<NetSession>().local_addr(), None);
    let _ = std::fs::remove_dir_all(&dir);

    // On: one port, both layers, each with the host's key.
    let dir = temp_dir("shared-on");
    write(&dir.join("server.toml"), SERVER_TOML);
    let mut host = host(&dir, &["--shared-port"]);
    let mut game = link_game();
    // The host's session runs on a manual clock too (no wall clock in the test).
    game.app
        .world_mut()
        .resource_mut::<NetSession>()
        .set_manual_clock(0);
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    assert!(host.game_owns_port(), "players and kick follow the link");
    let port_addr = host.game_addr().expect("bound");
    assert!(host.shared_port().is_some());
    let session_addr = game.app.world().resource::<NetSession>().local_addr();
    assert_eq!(
        session_addr,
        Some(port_addr),
        "the session listens on the game port"
    );
    let link_key = game.app.world().resource::<NetLink>().server_key();
    let session_key = game.app.world().resource::<NetSession>().server_key();
    assert!(link_key.is_some());
    assert_eq!(link_key, session_key, "one identity for both layers");

    let mut link_client = link_app(NetMode::Client);
    link_client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect_udp::<Wire>(
            port_addr,
            &LinkOptions {
                crypto: CryptoMode::Required,
                server_key: link_key,
                ..manual()
            },
        )
        .expect("connect");
    let manifest = game.app.world().resource::<NetSession>().manifest().clone();
    let mut session_client = NetSession::new(
        SessionSettings {
            encryption: CryptoMode::Required,
            ..SessionSettings::default()
        },
        manifest,
    );
    session_client.set_manual_clock(0);
    session_client
        .connect(
            ConnectTarget::Address(port_addr),
            ConnectOptions {
                server_key: session_key,
                ..ConnectOptions::default()
            },
        )
        .expect("connect");
    let mut joined = (false, false);
    for _ in 0..600 {
        game.app
            .world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        assert_eq!(host.run_once(&mut game), None);
        link_client.update();
        session_client.advance_manual_clock(16);
        session_client.update();
        joined = (
            !host
                .execute(&mut game, "players")
                .iter()
                .any(|l| l == "no players"),
            session_client.state().as_str() == "in_game",
        );
        if joined == (true, true) {
            break;
        }
    }
    assert_eq!(
        joined,
        (true, true),
        "session client state {}",
        session_client.state().as_str()
    );
    assert!(session_client.link_sealed(), "the session link is sealed too");
    let stats = host.shared_port().map(SharedPort::stats).expect("shared");
    assert_eq!(stats.routes, 2, "{stats:?}");

    // The console counts and lists both layers, and a bare id on both is refused.
    let status = host.execute(&mut game, "status").join(" ");
    assert!(status.contains("players=2/"), "{status}");
    let players = host.execute(&mut game, "players");
    assert_eq!(players.len(), 2, "{players:?}");
    assert!(players.iter().any(|l| l.ends_with("game-port")), "{players:?}");
    let session_line = players
        .iter()
        .find(|l| l.starts_with("session:"))
        .expect("the session's player is listed");
    let session_id = session_line.split_whitespace().next().expect("id");
    let bare = session_id.trim_start_matches("session:");
    let kick = host.execute(&mut game, &format!("kick {bare}")).join(" ");
    assert!(kick.contains("player_ambiguous"), "{kick}");
    assert_eq!(
        game.app
            .world()
            .resource::<NetSession>()
            .server()
            .map(|s| s.peer_ids().len()),
        Some(1),
        "a refused kick removes nobody"
    );
    // The line's first word works as it stands.
    host.execute(&mut game, &format!("kick {session_id}"));
    assert_eq!(
        game.app
            .world()
            .resource::<NetSession>()
            .server()
            .map(|s| s.peer_ids().len()),
        Some(0),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
