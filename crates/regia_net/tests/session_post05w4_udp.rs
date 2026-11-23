// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 6.2 over real UDP: this test binary re-runs itself once as the server and once as
//! the client (`REGIA_NET_P05W4_ROLE`), so the travel runs over real loopback sockets, real
//! process boundaries and the real session handshake (including the content manifest check the
//! parent lane added: both sides run the same mod version, so the join must not be refused).
//!
//! The parent never sleeps or waits on a clock: it reads the server's port lines and then waits
//! for the children to exit, each bounded by its own iteration count. Each child paces its loop
//! with a short sleep (pacing a real-time socket, not synchronizing with the other process); the
//! clocks are manual, stepped per iteration.
//!
//! Server: hosts the session on one UDP socket and the replication link on a second, spawns one
//! persistent and three plain replicated entities, and travels once the client signals (a
//! to-server message) that it holds the copies — deterministic in protocol terms, since the two
//! children pace themselves independently. Client: joins both sockets, asks for the travel once
//! it holds the four copies, checks every iteration after `TravelBegin` that no old-level copy
//! came back, rejoins the new level and reports what it ended holding.

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use regia_app::prelude::*;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::message::register_net_message;
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::session::lifecycle::{ClientSessionEvent, ClientSessionState};
use regia_net::session::net_session::{ConnectOptions, NetSessionEvents};
use regia_net::session::{ContentManifest, HostOptions, ManifestAsset, NetSession, SessionSettings};
use regia_net::socket::UdpTransport;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// The client tells the server it holds the old level's copies: travel on this signal, not on
/// a wall-clock guess (the two children pace themselves independently).
#[derive(Clone, Debug, PartialEq, Replicate)]
struct TravelPlease {
    n: u8,
}

impl NetMessage for TravelPlease {
    const DIRECTION: Direction = Direction::ToServer;
    fn validate(&self, _cx: &ValidateContext) -> Verdict {
        Verdict::Accept
    }
}
impl ToServerMessage for TravelPlease {}

/// The link's game: replication frames over the second socket.
struct Wire;
impl NetGame for Wire {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.post05w4.travel";
    const PROTOCOL_VERSION: u32 = 1;
}

const ROLE_ENV: &str = "REGIA_NET_P05W4_ROLE";
/// Loop iterations a child runs at most (a few milliseconds each).
const MAX_ITERATIONS: u32 = 20_000;
/// Iterations a child settles after the travel before reporting.
const SETTLE_STEPS: u32 = 120;

fn child_command(role: &str) -> Command {
    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", "udp_child", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd
}

/// The child entry point; a no-op in the parent run.
#[test]
fn udp_child() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    if role == "server" {
        child_server();
    } else if let Some(port) = role.strip_prefix("client:") {
        let mut parts = port.splitn(2, ':');
        let session_port: u16 = parts.next().and_then(|p| p.parse().ok()).expect("session port");
        let link_port: u16 = parts.next().and_then(|p| p.parse().ok()).expect("link port");
        child_client(session_port, link_port);
    }
}

fn link_opts() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 16_000,
        },
        ..LinkOptions::default()
    }
}

/// The manifest of the parent lane's check: both sides run the same paks and the same mod
/// version, so the handshake's content comparison must pass over the real socket.
fn asset(id: &str, kind: &str, hash: u64) -> ManifestAsset {
    ManifestAsset {
        id: id.into(),
        kind: kind.into(),
        content_hash: hash,
    }
}

fn manifest() -> ContentManifest {
    let base = vec![
        asset("levels/yard.level.ron", "level", 1),
        asset("config/rules.ron", "config", 2),
    ];
    let dlc = vec![asset("levels/harbor.level.ron", "level", 21)];
    let packs: Vec<(&str, &[ManifestAsset])> =
        vec![("base", base.as_slice()), ("dlc_harbor", dlc.as_slice())];
    ContentManifest::build("0.5.0", "1.2.0", regia_net::NET_PROTOCOL_VERSION, 0, &packs)
}

fn child_app(mode: NetMode) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetSessionPlugin, NetLinkPlugin));
    register_replicated::<Hp>(app.world_mut(), ReplicateOptions::default());
    register_net_message::<TravelPlease>(app.world_mut());
    app.world_mut()
        .resource_mut::<NetSettings>()
        .replication
        .send_every_ticks = 1;
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session.set_manifest(manifest());
    // Each child advances its manual clock 16 ms per ~2 ms of real time, so session time runs
    // about 8x real time; under gate load a peer can stall longer than the default timeouts
    // measure in real seconds (review rev-kimi-27 finding 3). Raise the ones this test can hit.
    session.set_settings(SessionSettings {
        connect_timeout_s: 600.0,
        stream_idle_timeout_s: 1_200.0,
        ..session.settings().clone()
    });
    app.insert_resource(session);
    app.finish_plugins();
    app
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn peer_in_game(app: &App) -> bool {
    app.world()
        .resource::<NetSession>()
        .server()
        .is_some_and(|s| s.peer_rows().iter().any(|r| r.state == "in_game"))
}

/// Whether the client's `TravelPlease` reached the server: travel on that signal.
fn travel_please_seen(app: &App) -> bool {
    let world = app.world();
    let reg = world.resource::<regia_net::message::NetMessageRegistry>();
    reg.index_of::<TravelPlease>()
        .is_some_and(|i| reg.counters[usize::from(i)].delivered > 0)
}

fn child_server() {
    let Ok(session_socket) = UdpTransport::bind(loopback(0)) else {
        println!("SKIP no loopback UDP");
        return;
    };
    let link_socket = UdpTransport::bind(loopback(0)).expect("bind link");
    let mut app = child_app(NetMode::DedicatedServer);
    let session_port = session_socket.local_addr().expect("addr").port();
    app.world_mut()
        .resource_mut::<NetSession>()
        .host_on(
            session_socket,
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
    app.world_mut()
        .resource_mut::<NetLink>()
        .host::<Wire>(Box::new(link_socket), &link_opts())
        .expect("link hosts");
    let link_port = app
        .world()
        .resource::<NetLink>()
        .local_addr()
        .expect("addr")
        .port();
    println!("SESSION_PORT={session_port}");
    println!("LINK_PORT={link_port}");

    let world = app.world_mut();
    world.spawn((rep(), Hp { hp: 9 }, NetPersistAcrossTravel));
    for i in 0..3 {
        world.spawn((rep(), Hp { hp: i }));
    }

    let mut travelled = false;
    let mut rejoined_at = None;
    for it in 0..MAX_ITERATIONS {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
        if !travelled && travel_please_seen(&app) {
            travelled = true;
            app.world_mut()
                .resource_mut::<NetSession>()
                .travel("levels/b.level.ron")
                .expect("travel");
        }
        if travelled && rejoined_at.is_none() && peer_in_game(&app) {
            rejoined_at = Some(it);
        }
        if rejoined_at.is_some_and(|at| it >= at + SETTLE_STEPS) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let rejoined = rejoined_at.is_some();
    let counts = app.world().resource::<NetSession>().travel_counts();
    assert!(travelled, "the peer never became in_game before the travel");
    assert!(rejoined, "the peer never rejoined after the travel");
    assert_eq!(
        (counts.despawned, counts.persistent),
        (3, 1),
        "the travel despawned the three plain entities and kept the persistent one"
    );
    println!(
        "SERVER_RESULT travel_despawned={} travel_persistent={}",
        counts.despawned, counts.persistent
    );
}

fn client_copies(app: &App) -> Vec<(NetId, Entity)> {
    app.world().resource::<NetEntities>().iter().collect()
}

fn child_client(session_port: u16, link_port: u16) {
    let mut app = child_app(NetMode::Client);
    let session_socket = UdpTransport::bind(loopback(0)).expect("bind session");
    let link_socket = UdpTransport::bind(loopback(0)).expect("bind link");
    app.world_mut()
        .resource_mut::<NetSession>()
        .connect_on(session_socket, loopback(session_port), ConnectOptions::default())
        .expect("connect");
    app.world_mut()
        .resource_mut::<NetLink>()
        .connect::<Wire>(Box::new(link_socket), loopback(link_port), &link_opts())
        .expect("link connects");

    let mut pre_travel: Vec<(NetId, Entity)> = Vec::new();
    let mut saw_travel = false;
    let mut kept_after: Option<(NetId, Entity)> = None;
    let mut most_after_travel = 0usize;
    let mut prev_copies: Vec<(NetId, Entity)> = Vec::new();
    let mut rejoined = false;
    let mut asked = false;
    for _it in 0..MAX_ITERATIONS {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
        let copies_now = client_copies(&app);
        if !asked && copies_now.len() == 4 {
            // The old level is fully replicated: ask the server to travel (deterministic in
            // protocol terms; the two children pace themselves independently).
            app.world_mut()
                .run_system_once(move |mut net: NetSender| {
                    net.send_session_to_server(TravelPlease { n: 1 }).expect("queued");
                })
                .expect("runs");
            asked = true;
        }
        // The session plugin's world-side system applies `TravelStarted` (the copy drop) inside
        // `app.update` and records it in `NetSessionEvents`; read that log instead of draining
        // the session's own event queue.
        let travel_now = app.world().get_resource::<NetSessionEvents>().is_some_and(|log| {
            log.client
                .iter()
                .any(|e| matches!(e, ClientSessionEvent::TravelStarted { .. }))
        });
        if travel_now && !saw_travel {
            saw_travel = true;
            // The drop ran inside this update; the frame before it held the old level.
            pre_travel = prev_copies.clone();
            kept_after = copies_now.first().copied();
        }
        if saw_travel {
            most_after_travel = most_after_travel.max(copies_now.len());
            if app.world().resource::<NetSession>().state() == ClientSessionState::InGame {
                rejoined = true;
                break;
            }
        }
        prev_copies = copies_now;
        std::thread::sleep(Duration::from_millis(2));
    }
    // Settle: late frames (the server's despawn records, retransmits) must not change anything.
    for _ in 0..SETTLE_STEPS {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
        if saw_travel {
            most_after_travel = most_after_travel.max(client_copies(&app).len());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        asked,
        "the client never held the four copies to ask for the travel"
    );
    assert_eq!(
        pre_travel.len(),
        4,
        "the client held all four copies before the travel"
    );
    assert!(saw_travel, "the client never saw TravelBegin");
    assert!(rejoined, "the client never rejoined after the travel");
    let (kept_id, kept_copy) = kept_after.expect("the persistent copy at TravelBegin");
    let state = app.world().resource::<NetSession>().state();
    assert_eq!(
        state,
        ClientSessionState::InGame,
        "the client ends in the new level: {state:?}"
    );
    let level = app
        .world()
        .resource::<NetSession>()
        .client()
        .map(|c| c.level().to_owned())
        .unwrap_or_default();
    assert_eq!(
        level, "levels/b.level.ron",
        "the client is in the travelled level"
    );
    // No old-level copy on any tick after the travel, and exactly the persistent copy remains,
    // on the same client entity it was before.
    assert_eq!(most_after_travel, 1, "an old-level copy showed after TravelBegin");
    let copies = client_copies(&app);
    assert_eq!(copies.len(), 1, "the client ends with exactly one copy");
    assert_eq!(copies[0], (kept_id, kept_copy), "the same persistent copy");
    for (id, _) in &pre_travel {
        if *id != kept_id {
            assert!(
                app.world().resource::<NetEntities>().entity(*id).is_none(),
                "old-level copy {id:?} is gone"
            );
        }
    }
    let dropped = app.world().resource::<NetSession>().client_travel_counts();
    assert_eq!((dropped.despawned, dropped.persistent), (3, 1));
    let mods = app.world().resource::<NetSession>().session_manifest().mods;
    println!(
        "CLIENT_RESULT copies=1 travel_despawned={} travel_persistent={} mods={}",
        dropped.despawned,
        dropped.persistent,
        mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(",")
    );
    // Say goodbye so the server sees an orderly leave.
    app.world_mut().resource_mut::<NetSession>().disconnect();
    for _ in 0..20 {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
    }
}

/// The first line holding `prefix`, from the prefix on (libtest may print `test udp_child ... `
/// on the same line before the child's own output).
fn result_line(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.find(prefix).map(|i| l[i..].to_owned()))
}

fn field(line: &str, key: &str) -> u64 {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

#[test]
fn two_processes_travel_over_real_udp() {
    if std::env::var(ROLE_ENV).is_ok() {
        return;
    }
    let mut server = child_command("server").spawn().expect("spawn server");
    let mut server_out = BufReader::new(server.stdout.take().expect("piped"));
    let mut session_port = None;
    let mut link_port = None;
    let mut line = String::new();
    // Bounded by the child: it prints its ports first, or exits (EOF) on failure.
    while server_out.read_line(&mut line).expect("read") > 0 {
        if let Some(p) = result_line(&line, "SESSION_PORT=") {
            session_port = p.trim().trim_start_matches("SESSION_PORT=").parse::<u16>().ok();
        }
        if let Some(p) = result_line(&line, "LINK_PORT=") {
            link_port = p.trim().trim_start_matches("LINK_PORT=").parse::<u16>().ok();
        }
        if line.contains("SKIP no loopback") {
            let _ = server.wait();
            return;
        }
        if session_port.is_some() && link_port.is_some() {
            break;
        }
        line.clear();
    }
    let session_port = session_port.expect("server printed its session port");
    let link_port = link_port.expect("server printed its link port");
    let client = child_command(&format!("client:{session_port}:{link_port}"))
        .spawn()
        .expect("spawn client");
    let out = client.wait_with_output().expect("client output");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "client failed:\n{text}");
    let client_line = result_line(&text, "CLIENT_RESULT").expect("client result line");
    assert_eq!(field(&client_line, "copies"), 1);
    assert_eq!(field(&client_line, "travel_despawned"), 3);
    assert_eq!(field(&client_line, "travel_persistent"), 1);

    let mut rest = String::new();
    server_out.read_to_string(&mut rest).expect("server output");
    let status = server.wait().expect("server exit");
    assert!(status.success(), "server failed:\n{rest}");
    let server_line = result_line(&rest, "SERVER_RESULT").expect("server result line");
    assert_eq!(field(&server_line, "travel_despawned"), 3);
    assert_eq!(field(&server_line, "travel_persistent"), 1);
}
