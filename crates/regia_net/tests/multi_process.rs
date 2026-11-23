// SPDX-License-Identifier: MPL-2.0
//! Three-process session over real UDP on 127.0.0.1: this test binary re-runs itself once as the
//! server and twice as clients (`REGIA_NET_MP_ROLE`), so the test exercises real sockets, real
//! process boundaries and real time.
//!
//! The parent never sleeps or waits on a clock: it reads the server's `PORT=` line from the
//! child's stdout and then waits for the children to exit. Each child bounds its own run by an
//! iteration count and paces its network loop with a short sleep (pacing a real-time loop, not
//! synchronizing with another process). The verdict is on counts the children print.

mod common;

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Duel, DuelMsg, Move, Team, simulate};
use regia_net::prelude::*;

const ROLE_ENV: &str = "REGIA_NET_MP_ROLE";
/// Loop iterations a child runs at most (about 2 ms each).
const MAX_ITERATIONS: u32 = 15_000;
/// Ticks the server simulates once both players are in.
const MATCH_TICKS: u64 = 90;

fn child_command(role: &str) -> Command {
    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", "mp_child", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd
}

/// The child entry point; a no-op in the parent run.
#[test]
fn mp_child() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    if role == "server" {
        child_server();
    } else if let Some(rest) = role.strip_prefix("client:") {
        let mut parts = rest.splitn(3, ':');
        let port: u16 = parts.next().and_then(|p| p.parse().ok()).expect("port");
        let name = parts.next().expect("name").to_owned();
        let team: u8 = parts.next().and_then(|t| t.parse().ok()).expect("team");
        child_client(port, &name, team);
    }
}

fn child_server() {
    let Ok(socket) = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
        println!("SKIP no loopback UDP");
        return;
    };
    let mut server: NetServer<Duel, _> = NetServer::new(socket, ServerConfig::default());
    let port = server.local_addr().expect("bound").port();
    println!("PORT={port}");
    let start = Instant::now();
    let mut last_ms = 0;
    let mut clock = TickClock::new(server.config().tick_rate_hz, 4);
    let mut positions = std::collections::BTreeMap::new();
    let mut chats = 0;
    let mut started_at_tick = None;
    let mut finished = false;
    for _ in 0..MAX_ITERATIONS {
        let now = start.elapsed().as_millis() as u64;
        server.receive(now);
        while let Some(event) = server.poll_event() {
            if let ServerEvent::Message {
                message: DuelMsg::Chat(_),
                ..
            } = event
            {
                chats += 1;
            }
        }
        if server.session().len() == 2 {
            let begin = *started_at_tick.get_or_insert(server.driver().tick());
            for _ in 0..clock.advance(now - last_ms) {
                let tick = server.driver().tick();
                server.advance_tick(|_, inputs| simulate(&mut positions, inputs));
                server.broadcast(
                    Channel::Unreliable,
                    &DuelMsg::State {
                        tick,
                        positions: positions
                            .iter()
                            .map(|(p, x): (&PeerId, &i32)| (p.0 as u16, *x))
                            .collect(),
                    },
                );
                if tick + 1 >= begin + MATCH_TICKS && !finished {
                    finished = true;
                    let leader = positions
                        .iter()
                        .max_by_key(|(_, x)| **x)
                        .map_or(0, |(p, _)| p.0 as u16);
                    server.broadcast(Channel::GAME_RPC, &DuelMsg::Winner(leader));
                }
            }
        }
        last_ms = now;
        server.flush(now);
        // Stop once both clients have left after the announcement.
        if finished && server.session().is_empty() {
            let missed: u64 = server.driver().peers().map(|p| server.driver().missed(p)).sum();
            println!(
                "SERVER_RESULT chats={chats} ticks={} accepted={} missed={missed}",
                server.driver().tick(),
                server.totals().accepted
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("server did not finish within its iteration budget");
}

fn child_client(port: u16, name: &str, team: u8) {
    let socket = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind client");
    let config = ClientConfig {
        name: name.into(),
        ..ClientConfig::default()
    };
    let mut client: NetClient<Duel, _> = NetClient::connect(
        socket,
        SocketAddr::from(([127, 0, 0, 1], port)),
        config,
        &Team(team),
    )
    .expect("join fits");
    let start = Instant::now();
    let mut chatted = false;
    let mut states = 0u32;
    let mut last_sent: Option<u64> = None;
    for _ in 0..MAX_ITERATIONS {
        let now = start.elapsed().as_millis() as u64;
        client.receive(now);
        while let Some(event) = client.poll_event() {
            match event {
                ClientEvent::Message {
                    message: DuelMsg::State { .. },
                    ..
                } => states += 1,
                ClientEvent::Message {
                    message: DuelMsg::Winner(w),
                    ..
                } => {
                    println!(
                        "CLIENT_RESULT name={name} peer={} winner={w} states={states}",
                        client.peer_id().map_or(0, |p| p.0)
                    );
                    client.disconnect();
                    client.flush(now);
                    return;
                }
                ClientEvent::Ended(end) => panic!("client {name} ended early: {}", end.as_str()),
                _ => {}
            }
        }
        if client.is_connected() {
            if !chatted {
                client
                    .send(Channel::GAME_RPC, &DuelMsg::Chat(format!("hi from {name}")))
                    .expect("queued");
                chatted = true;
            }
            let target = client.estimated_server_tick(now) + 4;
            let from = last_sent.map_or(target, |t| t + 1);
            for tick in from..=target {
                client.send_input(tick, Move(1)).expect("connected");
            }
            if from <= target {
                last_sent = Some(target);
            }
        }
        client.flush(now);
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("client {name} did not finish within its iteration budget");
}

/// The first line holding `prefix`, from the prefix on (libtest may print `test mp_child ... `
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
fn server_and_two_clients_in_three_processes() {
    if std::env::var(ROLE_ENV).is_ok() {
        return;
    }
    let mut server = child_command("server").spawn().expect("spawn server");
    let mut server_out = BufReader::new(server.stdout.take().expect("piped"));
    let mut port = None;
    let mut line = String::new();
    // Bounded by the child: it prints the port first, or exits (EOF) on failure.
    while server_out.read_line(&mut line).expect("read") > 0 {
        if let Some(p) = result_line(&line, "PORT=") {
            port = p.trim().trim_start_matches("PORT=").parse::<u16>().ok();
            break;
        }
        if line.contains("SKIP no loopback") {
            let _ = server.wait();
            return;
        }
        line.clear();
    }
    let port = port.expect("server printed its port");
    let clients: Vec<_> = [("ada", 0), ("bob", 1)]
        .into_iter()
        .map(|(name, team)| {
            child_command(&format!("client:{port}:{name}:{team}"))
                .spawn()
                .expect("spawn client")
        })
        .collect();
    let mut client_lines = Vec::new();
    for child in clients {
        let out = child.wait_with_output().expect("client output");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "client failed:\n{text}");
        client_lines.push(result_line(&text, "CLIENT_RESULT").expect("client result line"));
    }
    let mut rest = String::new();
    server_out.read_to_string(&mut rest).expect("server output");
    let status = server.wait().expect("server exit");
    assert!(status.success(), "server failed:\n{rest}");
    let server_line = result_line(&rest, "SERVER_RESULT").expect("server result line");

    assert_eq!(field(&server_line, "accepted"), 2);
    assert_eq!(field(&server_line, "chats"), 2, "both reliable chats arrived");
    assert!(field(&server_line, "ticks") >= MATCH_TICKS);
    let peers: Vec<u64> = client_lines.iter().map(|l| field(l, "peer")).collect();
    assert_ne!(peers[0], peers[1], "distinct peer ids");
    let winners: Vec<u64> = client_lines.iter().map(|l| field(l, "winner")).collect();
    assert_eq!(winners[0], winners[1], "both clients heard the same winner");
    assert!(peers.contains(&winners[0]), "the winner is one of the players");
    for l in &client_lines {
        assert!(field(l, "states") > 0, "state broadcasts reached {l}");
    }
}
