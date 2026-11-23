// SPDX-License-Identifier: MPL-2.0
//! Spec 76 § 10 and § 11 (tests 1 and 5-11, runtime half) in process, deterministic (ADR-2440):
//! the counterparts of `regia_cli`'s `server_package_e2e` checks that need no build of a game.
//! Every port is picked by the OS (`HostTransport::UdpAnyPort`, `status.port = 0`,
//! `--relay local:0`); time is the host's test clock and the clients' manual clocks; nothing sleeps
//! and nothing waits on the wall clock.
//!
//! - a session game: running, `/health`, a client that joins by join code, drain to exit 0;
//! - a replication game (`HostedLink`): the link binds the game port, `players`, `kick`, drain;
//! - the start failures of both kinds and their exit codes (2, 3).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::model::NetMode;
use regia_net::server_host::config::CliOverrides;
use regia_net::server_host::{
    AppHostGame, ExitCode, HostClock, HostGame, HostState, HostTransport, HostedLink, JoinResolver,
    JoinTicket, ServerHost, SessionOnlyGame, TestClock,
};
use regia_net::session::NetSession;
use regia_net::session::lifecycle::SessionSettings;
use regia_net::session::lifecycle::{ClientSessionState, PeerState, ServerSessionEvent};
use regia_net::session::net_session::{ConnectOptions, ConnectTarget};
use regia_net::{CryptoMode, NetGame, NetPlugin};

/// Real UDP session clients use the host's clock, not elapsed wall time.
fn step_session(host: &mut ServerHost, game: &mut SessionOnlyGame, client: &mut NetSession) {
    client.advance_manual_clock(16);
    client.update();
    assert_eq!(host.run_once(game), None);
    client.update();
}

fn session_client(game: &SessionOnlyGame) -> NetSession {
    let mut client = NetSession::new(
        SessionSettings {
            encryption: CryptoMode::Required,
            ..SessionSettings::default()
        },
        game.session.manifest().clone(),
    );
    client.set_manual_clock(0);
    client
}

fn connect_session(client: &mut NetSession, game: &SessionOnlyGame, resume: bool) {
    client
        .connect(
            ConnectTarget::Address(game.session.local_addr().expect("server bound")),
            ConnectOptions {
                server_key: game.session.server_key(),
                resume,
                ..ConnectOptions::default()
            },
        )
        .expect("connect");
}

#[test]
fn udp_host_refuses_content_and_protocol_mismatches_before_joining() {
    let dir = unique_temp_dir("udp-rejection");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &[], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock);
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    for reason in ["content_mismatch", "protocol_mismatch"] {
        let mut manifest = game.session.manifest().clone();
        if reason == "content_mismatch" {
            manifest.content_hash ^= 1;
            manifest.packs.push(("different-gameplay".into(), 1));
        } else {
            manifest.protocol_hash ^= 1;
        }
        let mut client = NetSession::new(
            SessionSettings {
                encryption: CryptoMode::Required,
                ..SessionSettings::default()
            },
            manifest,
        );
        client.set_manual_clock(0);
        connect_session(&mut client, &game, false);
        for _ in 0..600 {
            step_session(&mut host, &mut game, &mut client);
            assert_ne!(client.state(), ClientSessionState::InGame);
            if matches!(client.state(), ClientSessionState::Disconnected(Some(_))) {
                break;
            }
        }
        let ClientSessionState::Disconnected(Some(got)) = client.state() else {
            panic!("client was not refused: {:?}", client.state());
        };
        assert_eq!(got.code(), reason);
        assert!(game.session.server().expect("server").peer_ids().is_empty());
        assert!(game.session.take_server_events().iter().any(|event| {
            matches!(event, ServerSessionEvent::Refused { reason: got, .. } if got.code() == reason)
        }));
    }
    // A refused join must leave the host able to admit a valid sealed client.
    let mut good = session_client(&game);
    connect_session(&mut good, &game, false);
    for _ in 0..600 {
        step_session(&mut host, &mut game, &mut good);
        if good.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(good.state(), ClientSessionState::InGame);
    assert!(good.link_sealed());
    drop(host);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn udp_host_resumes_a_suspended_client_in_the_same_reserved_slot() {
    let dir = unique_temp_dir("udp-resume");
    write(
        &dir.join("server.toml"),
        &toml("").replace("max_players = 8", "max_players = 1"),
    );
    let clock = TestClock::new();
    let mut host = host(&dir, &[], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock);
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    let mut client = session_client(&game);
    connect_session(&mut client, &game, false);
    for _ in 0..600 {
        step_session(&mut host, &mut game, &mut client);
        if client.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    assert!(client.link_sealed());
    let peer = client.client().and_then(|c| c.peer_id()).expect("peer");
    let first_token = client.client().and_then(|c| c.reconnect_token()).expect("token");
    client.suspend();
    for _ in 0..600 {
        step_session(&mut host, &mut game, &mut client);
        if matches!(client.state(), ClientSessionState::Disconnected(_))
            && game.session.peer_state(peer) == Some(PeerState::Grace)
        {
            break;
        }
    }
    assert_eq!(game.session.peer_state(peer), Some(PeerState::Grace));
    client.reset();
    connect_session(&mut client, &game, true);
    for _ in 0..600 {
        step_session(&mut host, &mut game, &mut client);
        if client.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    assert!(client.link_sealed());
    assert_eq!(client.client().and_then(|c| c.peer_id()), Some(peer));
    assert_ne!(
        client.client().and_then(|c| c.reconnect_token()),
        Some(first_token)
    );
    assert_eq!(game.session.server().expect("server").peer_ids(), vec![peer]);
    assert!(
        game.session.take_server_events().iter().any(|event| {
            matches!(event, ServerSessionEvent::PlayerResumed { peer: got } if *got == peer)
        })
    );
    drop(host);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn udp_host_rejects_a_truncated_manifest_over_a_conditioned_sealed_link() {
    use regia_net::client::{ClientConfig, NetClient};
    use regia_net::connection::Channel;
    use regia_net::session::net_session::SessionWire;
    use regia_net::session::proto::{HELLO_PREFIX_BYTES, Hello};
    use regia_net::{ConditionerConfig, UdpTransport};

    let dir = unique_temp_dir("udp-malformed");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &[], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock.clone());
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    let mut client = NetClient::<SessionWire, _>::connect(
        UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        game.session.local_addr().expect("server bound"),
        ClientConfig {
            crypto: CryptoMode::Required,
            server_key: game.session.server_key(),
            conditioner: ConditionerConfig::parse_spec(
                "delay=20,jitter=5,reorder=0.2,reorder_max=40,seed=19",
            )
            .expect("conditioner"),
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    for _ in 0..600 {
        client.update(clock.now().as_millis() as u64);
        assert_eq!(host.run_once(&mut game), None);
        if client.is_connected() {
            break;
        }
    }
    assert!(client.is_connected() && client.is_sealed());
    let manifest = game.session.manifest().clone();
    let hello = Hello {
        protocol_version: regia_net::settings::MIN_NET_PROTOCOL_VERSION,
        protocol_hash: manifest.protocol_hash,
        manifest,
        player_names: vec!["malformed".into()],
        password: None,
        auth: None,
        client_rate_bytes: 49_152,
        reconnect_token: None,
    };
    let mut bytes = hello.encode().expect("valid hello");
    // Keep the compatible protocol prefix and only part of the manifest's first field.
    bytes.truncate(HELLO_PREFIX_BYTES + 1);
    client.send(Channel::GAME_RPC, &bytes).expect("send");
    let mut refused = false;
    for _ in 0..600 {
        client.update(clock.now().as_millis() as u64);
        assert_eq!(host.run_once(&mut game), None);
        refused |= game.session.take_server_events().iter().any(|event| {
            matches!(event, ServerSessionEvent::Refused { reason, .. } if reason.code() == "malformed_packet")
        });
        if refused {
            break;
        }
    }
    assert!(refused, "a truncated content manifest must be refused");
    assert!(game.session.server().expect("server").peer_ids().is_empty());
    assert!(client.conditioner().stats().delayed > 0);
    assert_eq!(client.conditioner().config().seed, 19);
    drop(host);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia_host_e2e_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// `server.toml` on loopback, encryption required, `/health` on a free port.
fn toml(extra_server: &str) -> String {
    format!(
        r#"
format_version = 1
[server]
name = "E2E"
bind = "127.0.0.1"
port = 7790
max_players = 8
tick_hz = 60
{extra_server}
[levels]
rotation = ["levels/a.level.ron"]
[net]
encryption = "required"
identity_file = "server.key"
[status]
enabled = true
bind = "127.0.0.1"
port = 0
[logs]
dir = "logs"
"#
    )
}

fn host(dir: &Path, cli: &[&str], clock: &TestClock) -> ServerHost {
    let cli = CliOverrides::parse(&cli.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).expect("cli");
    ServerHost::new(dir, cli, Box::new(|_| None), Box::new(clock.clone())).expect("config")
}

fn http_health(addr: SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("send");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// Ticks the host until the registrar has a join code; returns it and the relay.
fn join_code(host: &mut ServerHost, game: &mut dyn HostGame) -> (String, SocketAddr) {
    for _ in 0..30 {
        if let (Some(code), Some(relay)) = (host.join_code(), host.relay_addr()) {
            return (code.to_owned(), relay);
        }
        assert_eq!(host.run_once(game), None);
    }
    panic!("no join code within 30 ticks");
}

/// Resolves `code` on `relay`, ticking the host (which runs the local relay) between lookups.
fn resolve(
    host: &mut ServerHost,
    game: &mut dyn HostGame,
    clock: &TestClock,
    relay: SocketAddr,
    code: &str,
) -> JoinTicket {
    let mut resolver = JoinResolver::udp(relay, code).expect("resolver");
    for _ in 0..30 {
        if let Some(done) = resolver.update(clock.now().as_millis() as u64) {
            return done.expect("resolved");
        }
        assert_eq!(host.run_once(game), None);
    }
    panic!("not resolved within 30 ticks");
}

/// Spec 76 § 10 and § 11 test 1 (runtime half): a session game on an OS-picked port reports
/// `running`, a client finds it by join code and joins (`"players":1`), and a shutdown request
/// drains it to exit code 0.
#[test]
fn a_session_server_joins_by_code_reports_health_and_drains_to_exit_0() {
    let dir = unique_temp_dir("session");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &["--relay", "local:0"], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock.clone());
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    assert!(
        !host.game_owns_port(),
        "a session game: the session binds the port"
    );
    let addr = host.game_addr().expect("bound");
    assert_ne!(addr.port(), 0);
    assert_eq!(game.session.local_addr(), Some(addr));
    let health = host.health_addr().expect("health bound");
    let text = http_health(health);
    assert!(text.contains("\"state\":\"running\""), "{text}");

    let (code, relay) = join_code(&mut host, &mut game);
    // The local relay speaks the current relay protocol (3 since ADR-8340: NAT traversal
    // introductions on top of protocol 2's allocations) and has its allocation pool (spec 120
    // § 10.6).
    let status = host.relay_status().expect("a local relay");
    assert_eq!(
        (status.protocol, status.lobbies),
        (regia_net::relay::RELAY_PROTOCOL_VERSION, 1)
    );
    assert_eq!(
        status.ports,
        usize::from(regia_net::server_host::LOCAL_RELAY_ALLOCATION_PORTS)
    );
    let ticket = resolve(&mut host, &mut game, &clock, relay, &code);
    assert_eq!(ticket.addr, addr);
    assert_eq!(
        ticket.server_key,
        game.session.server_key(),
        "the ticket pins the session's key"
    );

    let mut client = NetSession::new(
        SessionSettings {
            encryption: CryptoMode::Required,
            ..SessionSettings::default()
        },
        game.session.manifest().clone(),
    );
    client.set_manual_clock(0);
    client
        .connect(
            ConnectTarget::Address(ticket.addr),
            ConnectOptions {
                server_key: ticket.server_key,
                ..ConnectOptions::default()
            },
        )
        .expect("connect");
    let mut joined = false;
    for _ in 0..300 {
        client.advance_manual_clock(16);
        client.update();
        assert_eq!(host.run_once(&mut game), None);
        client.update();
        if client.state().as_str() == "in_game" {
            joined = true;
            break;
        }
    }
    assert!(joined, "the client joined: {}", client.state().as_str());
    assert!(client.link_sealed(), "encryption required: the link is sealed");
    for _ in 0..90 {
        assert_eq!(host.run_once(&mut game), None);
    }
    let text = http_health(health);
    assert!(text.contains("\"players\":1"), "{text}");
    assert!(text.contains(&format!("\"game_port\":\"{addr}\"")), "{text}");
    assert!(text.contains(&format!("\"join_code\":\"{code}\"")), "{text}");

    host.signals().request_shutdown();
    let mut code = None;
    for _ in 0..600 {
        client.advance_manual_clock(16);
        client.update();
        if let Some(c) = host.run_once(&mut game) {
            code = Some(c);
            break;
        }
    }
    assert_eq!(code, Some(ExitCode::Ok), "drained and stopped");
    assert_eq!(host.state(), HostState::Stopped);
    let _ = std::fs::remove_dir_all(&dir);
}

struct Wire;

impl NetGame for Wire {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.host_e2e";
    const PROTOCOL_VERSION: u32 = 1;
}

fn manual() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
}

fn link_app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

fn link_game() -> AppHostGame {
    let mut app = link_app(NetMode::DedicatedServer);
    app.insert_resource(HostedLink::new::<Wire>(LinkOptions {
        clock: manual(),
        ..LinkOptions::default()
    }));
    AppHostGame::new(app)
}

/// A replication game: the link binds the game port with the host's settings; a client joins,
/// shows in `players`, is kicked by the console, and the drain closes the link.
#[test]
fn a_link_server_lists_kicks_and_drains_its_peers() {
    let dir = unique_temp_dir("link");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &[], &clock);
    let mut game = link_game();
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    assert!(host.game_owns_port());
    let addr = host.game_addr().expect("bound");
    let key = game.app.world().resource::<NetLink>().server_key();
    assert!(key.is_some(), "encryption required: the link has the host's key");
    let line = host
        .take_console_output()
        .into_iter()
        .find(|l| l.starts_with("listening "))
        .expect("listening line");
    assert!(line.contains(&addr.to_string()), "{line}");
    assert_eq!(
        host.execute(&mut game, "serverkey"),
        [format!("server key: {}", key.expect("key"))]
    );

    let mut client = link_app(NetMode::Client);
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect_udp::<Wire>(
            addr,
            &LinkOptions {
                crypto: CryptoMode::Required,
                server_key: key,
                clock: manual(),
                ..LinkOptions::default()
            },
        )
        .expect("connect");
    let mut players = Vec::new();
    for _ in 0..300 {
        assert_eq!(host.run_once(&mut game), None);
        client.update();
        players = host.execute(&mut game, "players");
        if players.len() == 1 && players[0].ends_with(" game-port") {
            break;
        }
    }
    assert_eq!(players.len(), 1, "{players:?}");
    let status = host.execute(&mut game, "status");
    assert!(status[0].contains("players=1/8"), "{status:?}");
    let peer = players[0].split_whitespace().next().expect("peer id").to_owned();
    assert_eq!(
        host.execute(&mut game, &format!("kick {peer} bye")),
        [format!("kicked {peer}")]
    );
    assert_eq!(
        host.execute(&mut game, "kick 999"),
        ["error: player_unknown: 999".to_owned()]
    );
    let mut ended = None;
    for _ in 0..60 {
        assert_eq!(host.run_once(&mut game), None);
        client.update();
        let events = client.world_mut().resource_mut::<NetLink>().take_events();
        if let Some(reason) = events.into_iter().find_map(|e| match e {
            LinkEvent::Ended { reason } => Some(reason),
            _ => None,
        }) {
            ended = Some(reason);
            break;
        }
    }
    assert_eq!(ended.as_deref(), Some("kicked"), "the client hears why");

    host.signals().request_shutdown();
    let mut code = None;
    for _ in 0..30 {
        if let Some(c) = host.run_once(&mut game) {
            code = Some(c);
            break;
        }
    }
    assert_eq!(code, Some(ExitCode::Ok));
    assert_eq!(
        game.app.world().resource::<NetLink>().role(),
        "idle",
        "the drain closed the link"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 76 § 11 tests 9-11 for a replication game, and the join-code flags: a port in use is 3, a
/// bad `--relay` is 2, a local relay port in use is 3, a `HostedLink` without a `NetLink` is 3.
#[test]
fn link_game_start_failures_have_their_exit_codes() {
    let clock = TestClock::new();
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").expect("hold a port");
    let port = taken.local_addr().expect("addr").port();

    let dir = unique_temp_dir("fail-port");
    write(
        &dir.join("server.toml"),
        &toml("").replace("port = 7790", &format!("port = {port}")),
    );
    let mut h = host(&dir, &[], &clock);
    let e = h.start(&mut link_game(), HostTransport::Udp).expect_err("in use");
    assert_eq!(e.exit, ExitCode::Bind, "{e}");
    assert!(e.lines[0].contains("link_bind_failed"), "{e}");

    let dir = unique_temp_dir("fail-relay");
    write(&dir.join("server.toml"), &toml(""));
    let mut h = host(&dir, &["--relay", "local:x"], &clock);
    let e = h
        .start(&mut link_game(), HostTransport::UdpAnyPort)
        .expect_err("bad relay");
    assert_eq!(e.exit, ExitCode::Config, "{e}");

    let mut h = host(&dir, &["--relay", &format!("local:{port}")], &clock);
    let e = h
        .start(&mut link_game(), HostTransport::UdpAnyPort)
        .expect_err("relay port in use");
    assert_eq!(e.exit, ExitCode::Bind, "{e}");

    let mut app = App::new();
    app.insert_resource(HostedLink::new::<Wire>(LinkOptions::default()));
    let mut h = host(&dir, &[], &clock);
    let e = h
        .start(&mut AppHostGame::new(app), HostTransport::UdpAnyPort)
        .expect_err("no link");
    assert_eq!(e.exit, ExitCode::Bind, "{e}");
    assert!(e.lines[0].contains("link_missing"), "{e}");

    // Local transport: nothing binds, and the game keeps its link idle.
    let mut h = host(&dir, &["--relay", "local:0"], &clock);
    let mut game = link_game();
    h.start(&mut game, HostTransport::Local).expect("local");
    assert!(!h.game_owns_port());
    assert_eq!(h.game_addr(), None);
    assert_eq!(game.app.world().resource::<NetLink>().role(), "idle");
    drop(taken);
}

/// Spec 120 § 10.7 (ADR-8183, N113): a server with `--relay local:0` registers from its game
/// port (the relay records the game port's own address and the ticket says so), and a client
/// joins with `NetSession::connect(ConnectTarget::JoinCode("CODE@RELAY"))`; the host's relay
/// report carries the join code and the local relay's counters. On the base the registrar used a
/// socket of its own and `connect(JoinCode)` returned an error.
#[test]
fn a_session_server_registers_from_its_game_port_and_a_client_joins_by_code() {
    let dir = unique_temp_dir("relay-game-port");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &["--relay", "local:0"], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock.clone());
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    let addr = host.game_addr().expect("bound");
    let (code, relay) = join_code(&mut host, &mut game);
    let ticket = resolve(&mut host, &mut game, &clock, relay, &code);
    assert_eq!(ticket.addr, addr, "the relay saw the game port itself");
    let report = host.relay_report();
    assert_eq!(report.role, "host+relay");
    assert_eq!(report.join_code.as_deref(), Some(code.as_str()));
    assert_eq!(report.status.as_ref().map(|s| s.lobbies), Some(1));
    // The hosting flow's check (ADR-8370): a relay on this machine has nothing to map or measure,
    // and the report says so instead of leaving the check running.
    let check = report.host_check.expect("the host check runs with --relay");
    assert_eq!(check.mapping_error.as_deref(), Some("port_map_off"));
    assert_eq!(check.reachable, "no_mapping");
    assert!(check.done, "{check:?}");

    let mut client = NetSession::new(
        SessionSettings {
            encryption: CryptoMode::Required,
            ..SessionSettings::default()
        },
        game.session.manifest().clone(),
    );
    client.set_manual_clock(0);
    client
        .connect(
            ConnectTarget::JoinCode(format!("{code}@{relay}")),
            ConnectOptions::default(),
        )
        .expect("connect by code");
    let mut joined = false;
    for _ in 0..300 {
        client.advance_manual_clock(16);
        client.update();
        assert_eq!(host.run_once(&mut game), None);
        client.update();
        if client.state().as_str() == "in_game" {
            joined = true;
            break;
        }
    }
    assert!(joined, "the client joined by code: {}", client.state().as_str());
    assert!(
        client.link_sealed(),
        "the key from the code was pinned and the link sealed"
    );
    assert_eq!(client.server_key(), game.session.server_key());
    let _ = std::fs::remove_dir_all(&dir);
}

/// N-19 (ADR-10069): the console's `revokecode` withdraws the host's join code on the relay: the
/// relay drops the lobby, the host reports no code, and a lookup of the code is refused.
#[test]
fn the_console_revokecode_withdraws_the_join_code() {
    let dir = unique_temp_dir("relay-revoke");
    write(&dir.join("server.toml"), &toml(""));
    let clock = TestClock::new();
    let mut host = host(&dir, &["--relay", "local:0"], &clock);
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock.clone());
    host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
    let (code, relay) = join_code(&mut host, &mut game);
    assert_eq!(host.relay_status().map(|s| s.lobbies), Some(1));
    assert_eq!(
        host.execute(&mut game, "revokecode"),
        vec!["join code withdrawn".to_owned()]
    );
    assert_eq!(host.join_code(), None, "the host still reports the code");
    for _ in 0..30 {
        if host.join_code_revoked() {
            break;
        }
        clock.advance(std::time::Duration::from_millis(20));
        assert_eq!(host.run_once(&mut game), None);
    }
    assert!(host.join_code_revoked(), "the relay never confirmed");
    assert_eq!(
        host.relay_status().map(|s| s.lobbies),
        Some(0),
        "the relay kept the lobby"
    );
    let mut resolver = JoinResolver::udp(relay, &code).expect("resolver");
    let mut answer = None;
    for _ in 0..30 {
        if let Some(done) = resolver.update(clock.now().as_millis() as u64) {
            answer = Some(done);
            break;
        }
        assert_eq!(host.run_once(&mut game), None);
    }
    let err = answer
        .expect("the relay answered")
        .expect_err("the withdrawn code resolved");
    assert_eq!(err.code(), "join_refused", "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
