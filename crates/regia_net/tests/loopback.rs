// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests over the in-memory loopback network: handshake, refusals, messages under a
//! conditioner, the authoritative tick driver, timeouts, kicks, bans, the relay path and the
//! agent bridge. Time is a step counter (10 ms per step); nothing sleeps.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;

use common::{Duel, DuelMsg, DuelV2, Move, Team, simulate};
use regia_net::prelude::*;
use regia_net::relay::{
    RelayReply, RelayServer, encode_bind, encode_lookup, encode_register, encode_register_with_cookie,
};
use regia_net::{LoopbackSocket, NetStatus};

type Server = NetServer<Duel, LoopbackSocket>;
type Client = NetClient<Duel, LoopbackSocket>;

const STEP_MS: u64 = 10;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn server(net: &LoopbackNet, config: ServerConfig) -> Server {
    NetServer::with_secret(net.bind(addr(7777)).expect("bind server"), config, [7; 32])
}

fn client(net: &LoopbackNet, name: &str, team: u8, edit: impl FnOnce(&mut ClientConfig)) -> Client {
    let mut config = ClientConfig {
        name: name.into(),
        ..ClientConfig::default()
    };
    edit(&mut config);
    NetClient::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(7777),
        config,
        &Team(team),
    )
    .expect("join fits")
}

/// Steps every endpoint once at `now`.
fn step(server: &mut Server, clients: &mut [&mut Client], now: u64) {
    for c in clients.iter_mut() {
        c.update(now);
    }
    server.update(now);
}

fn client_events(c: &mut Client) -> Vec<ClientEvent<Duel>> {
    std::iter::from_fn(|| c.poll_event()).collect()
}

fn server_events(s: &mut Server) -> Vec<ServerEvent<Duel>> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

/// Steps until `done` or `max_steps`; returns the time reached.
fn run(
    server: &mut Server,
    clients: &mut [&mut Client],
    from: u64,
    max_steps: u64,
    mut done: impl FnMut(&Server, &[&mut Client]) -> bool,
) -> u64 {
    let mut now = from;
    for _ in 0..max_steps {
        if done(server, clients) {
            return now;
        }
        now += STEP_MS;
        step(server, clients, now);
    }
    now
}

#[test]
fn two_players_join_and_exchange_reliable_and_unreliable_messages() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut a = client(&net, "ada", 0, |_| {});
    let mut b = client(&net, "bob", 1, |_| {});
    let now = run(&mut s, &mut [&mut a, &mut b], 0, 100, |_, cs| {
        cs.iter().all(|c| c.is_connected())
    });
    assert!(a.is_connected() && b.is_connected(), "both connected");
    assert_ne!(a.peer_id(), b.peer_id());
    let joined: Vec<(String, u8)> = server_events(&mut s)
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::PeerJoined { name, join, .. } => Some((name, join.0)),
            _ => None,
        })
        .collect();
    assert_eq!(joined, vec![("ada".into(), 0), ("bob".into(), 1)]);

    a.send(Channel::GAME_RPC, &DuelMsg::Chat("hello".into()))
        .expect("queued");
    s.broadcast(
        Channel::Unreliable,
        &DuelMsg::State {
            tick: 1,
            positions: vec![(1, 2)],
        },
    );
    let a_peer = a.peer_id().expect("connected");
    s.send(a_peer, Channel::GAME_RPC, &DuelMsg::Winner(a_peer.0 as u16))
        .expect("queued");
    run(&mut s, &mut [&mut a, &mut b], now, 20, |_, _| false);
    let got: Vec<DuelMsg> = server_events(&mut s)
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(got, vec![DuelMsg::Chat("hello".into())]);
    let a_msgs: Vec<DuelMsg> = client_events(&mut a)
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert!(a_msgs.contains(&DuelMsg::Winner(a_peer.0 as u16)));
    assert!(a_msgs.contains(&DuelMsg::State {
        tick: 1,
        positions: vec![(1, 2)]
    }));
}

#[test]
fn reliable_stream_survives_a_stressed_link_in_order() {
    let net = LoopbackNet::new();
    let stress = ConditionerConfig::from_profile("wan_stress").expect("profile");
    let mut s = server(
        &net,
        ServerConfig {
            conditioner: ConditionerConfig {
                seed: 11,
                ..stress.clone()
            },
            ..ServerConfig::default()
        },
    );
    let mut a = client(&net, "ada", 0, |c| {
        c.conditioner = ConditionerConfig { seed: 12, ..stress }
    });
    let now = run(&mut s, &mut [&mut a], 0, 500, |_, cs| cs[0].is_connected());
    assert!(a.is_connected(), "handshake retries through 8 % loss");
    for i in 0..300 {
        a.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("line {i}")))
            .expect("queued");
    }
    run(&mut s, &mut [&mut a], now, 3_000, |_, _| false);
    let lines: Vec<String> = server_events(&mut s)
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::Message {
                message: DuelMsg::Chat(t),
                ..
            } => Some(t),
            _ => None,
        })
        .collect();
    let expect: Vec<String> = (0..300).map(|i| format!("line {i}")).collect();
    assert_eq!(lines, expect, "every reliable message once, in order");
    let rows = s.peer_rows();
    assert!(rows[0].link.packets_lost > 0, "the conditioner did drop packets");
    assert!(s.conditioner().stats().dropped > 0);
}

/// One stressed session: returns the server's per-peer link counters, its conditioner counters and
/// how many events it produced.
/// Per peer: packets sent, received, lost and reliable resends.
type LinkCounts = Vec<(u64, u64, u64, u64)>;

fn stressed_session() -> (LinkCounts, regia_net::ConditionerStats, usize) {
    let net = LoopbackNet::new();
    let stress = ConditionerConfig::from_profile("wan_stress").expect("profile");
    let mut s = server(
        &net,
        ServerConfig {
            conditioner: ConditionerConfig {
                seed: 21,
                ..stress.clone()
            },
            ..ServerConfig::default()
        },
    );
    let mut a = client(&net, "ada", 0, |c| {
        c.conditioner = ConditionerConfig { seed: 22, ..stress }
    });
    let now = run(&mut s, &mut [&mut a], 0, 500, |_, cs| cs[0].is_connected());
    for i in 0..100 {
        a.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("{i}")))
            .expect("queued");
    }
    run(&mut s, &mut [&mut a], now, 1_000, |_, _| false);
    let rows = s
        .peer_rows()
        .iter()
        .map(|r| {
            (
                r.link.packets_sent,
                r.link.packets_received,
                r.link.packets_lost,
                r.link.reliable_resends,
            )
        })
        .collect();
    (rows, s.conditioner().stats(), server_events(&mut s).len())
}

#[test]
fn same_seeds_replay_the_same_session() {
    let first = stressed_session();
    assert!(first.1.dropped > 0, "the conditioner was active");
    assert_eq!(first, stressed_session(), "same seeds and traffic, same session");
}

#[test]
fn authoritative_ticks_collect_redundant_inputs_without_misses() {
    let net = LoopbackNet::new();
    let wan = ConditionerConfig::from_profile("wan").expect("profile");
    let mut s = server(
        &net,
        ServerConfig {
            conditioner: ConditionerConfig {
                seed: 3,
                ..wan.clone()
            },
            ..ServerConfig::default()
        },
    );
    let mut a = client(&net, "ada", 0, |c| {
        c.conditioner = ConditionerConfig {
            seed: 4,
            ..wan.clone()
        }
    });
    let mut b = client(&net, "bob", 1, |c| {
        c.conditioner = ConditionerConfig { seed: 5, ..wan }
    });
    let mut now = run(&mut s, &mut [&mut a, &mut b], 0, 500, |_, cs| {
        cs.iter().all(|c| c.is_connected())
    });
    assert!(a.is_connected() && b.is_connected());
    let mut clock = TickClock::new(s.config().tick_rate_hz, 4);
    let mut positions: BTreeMap<PeerId, i32> = BTreeMap::new();
    let lead = 8;
    let mut last_sent = [None::<u64>; 2];
    let mut missed_at_warmup = None;
    for i in 0..1_500u64 {
        now += STEP_MS;
        for (k, c) in [&mut a, &mut b].into_iter().enumerate() {
            c.receive(now);
            let target = c.estimated_server_tick(now) + lead;
            // Send each tick's input once, in order; a jump in the estimate fills the gap.
            let from = last_sent[k].map_or(target, |t| t + 1);
            for tick in from..=target {
                c.send_input(tick, Move(if k == 0 { 1 } else { -1 }))
                    .expect("connected");
            }
            if from <= target {
                last_sent[k] = Some(target);
            }
            c.flush(now);
        }
        s.receive(now);
        for _ in 0..clock.advance(STEP_MS) {
            s.advance_tick(|_, inputs| simulate(&mut positions, inputs));
        }
        s.flush(now);
        if i == 300 {
            missed_at_warmup = Some(s.peer_rows().iter().map(|r| r.missed_input_ticks).sum::<u64>());
        }
    }
    let missed_end: u64 = s.peer_rows().iter().map(|r| r.missed_input_ticks).sum();
    let warm = missed_at_warmup.expect("warmup recorded");
    assert_eq!(
        missed_end, warm,
        "after warm-up no tick lacked an input (2 % loss healed by redundancy)"
    );
    let pa = positions.get(&a.peer_id().expect("id")).copied().unwrap_or(0);
    let pb = positions.get(&b.peer_id().expect("id")).copied().unwrap_or(0);
    assert!(pa > 300 && pb < -300, "inputs drove the simulation: {pa} {pb}");
}

#[test]
fn password_build_protocol_join_data_and_capacity_refusals() {
    let net = LoopbackNet::new();
    let mut s = server(
        &net,
        ServerConfig {
            password: Some("sesame".into()),
            build: "1.2.3".into(),
            max_peers: 1,
            ..ServerConfig::default()
        },
    );
    let ok = |c: &mut ClientConfig| {
        c.password = Some("sesame".into());
        c.build = "1.2.3".into();
    };
    let mut wrong_pw = client(&net, "w", 0, |c| {
        ok(c);
        c.password = Some("nope".into());
    });
    let mut no_pw = client(&net, "n", 0, |c| c.build = "1.2.3".into());
    let mut wrong_build = client(&net, "b", 0, |c| {
        ok(c);
        c.build = "9.9.9".into();
    });
    let mut bad_team = client(&net, "t", 7, ok);
    let mut v2: NetClient<DuelV2, LoopbackSocket> = NetClient::connect(
        net.bind(addr(0)).expect("bind"),
        addr(7777),
        ClientConfig::default(),
        &Team(0),
    )
    .expect("fits");
    let mut good = client(&net, "g", 0, ok);
    let mut late = client(&net, "l", 0, ok);
    let mut now = 0;
    for _ in 0..100 {
        now += STEP_MS;
        for c in [
            &mut wrong_pw,
            &mut no_pw,
            &mut wrong_build,
            &mut bad_team,
            &mut good,
        ] {
            c.update(now);
        }
        v2.update(now);
        s.update(now);
    }
    let denied = |c: &Client| match c.state() {
        ClientState::Ended(ClientEnd::Denied { reason, .. }) => Some(*reason),
        _ => None,
    };
    assert_eq!(denied(&wrong_pw), Some(DenyReason::BadPassword));
    assert_eq!(denied(&no_pw), Some(DenyReason::BadPassword));
    assert_eq!(denied(&wrong_build), Some(DenyReason::BuildMismatch));
    assert_eq!(denied(&bad_team), Some(DenyReason::InvalidJoin));
    assert!(matches!(
        v2.state(),
        ClientState::Ended(ClientEnd::Denied {
            reason: DenyReason::ProtocolMismatch,
            ..
        })
    ));
    assert!(good.is_connected());
    for _ in 0..100 {
        now += STEP_MS;
        late.update(now);
        good.update(now);
        s.update(now);
    }
    assert_eq!(denied(&late), Some(DenyReason::Full));
    assert_eq!(s.session().len(), 1);
    assert_eq!(s.totals().accepted, 1);
}

#[test]
fn silence_times_out_on_both_sides() {
    let net = LoopbackNet::new();
    // Keep the original 10 s deadline contract explicit; defaults now include 30 s startup grace.
    let mut s = server(
        &net,
        ServerConfig {
            health: regia_net::session::HealthPolicy {
                timeout_ms: 10_000,
                start_grace_ms: 0,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mut a = client(&net, "ada", 0, |c| {
        c.timeout_ms = 3_000;
        c.start_grace_ms = 0;
    });
    let now = run(&mut s, &mut [&mut a], 0, 100, |_, cs| cs[0].is_connected());
    server_events(&mut s);
    // The client stops talking; the server keeps updating.
    let mut t = now;
    for _ in 0..1_100 {
        t += STEP_MS;
        s.update(t);
    }
    let left: Vec<DisconnectReason> = server_events(&mut s)
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::PeerLeft { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(left, vec![DisconnectReason::Timeout]);
    // The client hears the server's timeout notice (or, if that is lost, silence for its own
    // timeout): either way it ends with `Timeout`.
    a.update(t);
    a.update(t + 3_000);
    assert!(matches!(
        a.state(),
        ClientState::Ended(ClientEnd::Disconnected(DisconnectReason::Timeout))
    ));
}

#[test]
fn kick_and_ban_reach_the_client_and_ban_blocks_rejoin() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut a = client(&net, "ada", 0, |_| {});
    let mut b = client(&net, "bob", 1, |_| {});
    let now = run(&mut s, &mut [&mut a, &mut b], 0, 100, |_, cs| {
        cs.iter().all(|c| c.is_connected())
    });
    assert!(s.kick(a.peer_id().expect("id"), DisconnectReason::Kicked));
    assert!(s.ban(b.peer_id().expect("id"), "test", 0));
    let now = run(&mut s, &mut [&mut a, &mut b], now, 10, |_, _| false);
    assert_eq!(
        a.state(),
        &ClientState::Ended(ClientEnd::Disconnected(DisconnectReason::Kicked))
    );
    assert_eq!(
        b.state(),
        &ClientState::Ended(ClientEnd::Disconnected(DisconnectReason::Banned))
    );
    let mut again = client(&net, "bob2", 0, |_| {});
    run(&mut s, &mut [&mut again], now, 100, |_, _| false);
    // Every loopback client shares 127.0.0.1, so the IP ban refuses the newcomer too.
    assert!(matches!(
        again.state(),
        ClientState::Ended(ClientEnd::Denied {
            reason: DenyReason::Banned,
            ..
        })
    ));
    assert_eq!(s.session().bans().len(), 1);
}

#[test]
fn spectators_join_but_their_input_is_ignored() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut spec = client(&net, "eye", 0, |c| c.role = PeerRole::Spectator);
    let mut now = run(&mut s, &mut [&mut spec], 0, 100, |_, cs| cs[0].is_connected());
    for tick in 0..20 {
        spec.send_input(tick, Move(1)).expect("connected");
        now += STEP_MS;
        step(&mut s, &mut [&mut spec], now);
    }
    assert_eq!(
        s.driver().peers().count(),
        0,
        "spectators are not in the tick driver"
    );
    assert_eq!(s.peer_rows()[0].role, "spectator");
}

#[test]
fn clean_disconnect_is_reported_as_left() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut a = client(&net, "ada", 0, |_| {});
    let now = run(&mut s, &mut [&mut a], 0, 100, |_, cs| cs[0].is_connected());
    server_events(&mut s);
    a.disconnect();
    a.flush(now + STEP_MS);
    s.update(now + STEP_MS);
    let left: Vec<DisconnectReason> = server_events(&mut s)
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::PeerLeft { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(left, vec![DisconnectReason::Left]);
    assert!(s.session().is_empty());
}

#[test]
fn garbage_datagrams_never_create_peers_or_panic() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut attacker = net.bind(addr(6666)).expect("bind");
    let mut rng = 0x1234_5678_u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for i in 0..5_000u64 {
        let len = (next() % 300) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        if i.is_multiple_of(2) && bytes.len() >= 5 {
            // Half of them carry our magic and a valid kind, to reach the decoders.
            bytes[..4].copy_from_slice(b"RGN1");
            bytes[4] = 1 + (next() % 7) as u8;
        }
        attacker.send_to(&bytes, addr(7777)).expect("send");
        if i.is_multiple_of(100) {
            s.update(i);
        }
    }
    s.update(10_000);
    assert!(s.session().is_empty(), "no peer without a valid cookie");
    assert!(s.totals().malformed + s.totals().foreign > 0);
}

#[test]
fn relay_join_code_and_forwarded_session() {
    let net = LoopbackNet::new();
    let relay_addr = addr(7800);
    let mut relay = RelayServer::new(net.bind(relay_addr).expect("bind relay"), 99);
    let mut s = server(&net, ServerConfig::default());
    // The host's relay leg: registered with the relay, it passes datagrams between the relay and
    // the game server (a host whose game socket cannot be reached directly).
    let mut host_leg = net.bind(addr(7001)).expect("bind host leg");
    host_leg
        .send_to(&encode_register(60, b"token").expect("fits"), relay_addr)
        .expect("send");
    relay.update(0);
    let mut buf = [0u8; 2048];
    let (n, _) = host_leg.recv_from(&mut buf).expect("io").expect("reply");
    let Some(RelayReply::RegisterChallenge(cookie)) = RelayReply::decode(&buf[..n]).expect("valid") else {
        panic!("expected a challenge");
    };
    host_leg
        .send_to(
            &encode_register_with_cookie(60, b"token", &cookie).expect("fits"),
            relay_addr,
        )
        .expect("send");
    relay.update(0);
    let (n, _) = host_leg.recv_from(&mut buf).expect("io").expect("reply");
    let Some(RelayReply::JoinCode(code)) = RelayReply::decode(&buf[..n]).expect("valid") else {
        panic!("expected a join code");
    };
    // The player looks the code up and binds a flow to the host, from its game socket.
    let mut player = net.bind(addr(7002)).expect("bind player");
    player
        .send_to(&encode_lookup(&code.to_lowercase()).expect("code"), relay_addr)
        .expect("send");
    relay.update(1);
    let (n, _) = player.recv_from(&mut buf).expect("io").expect("reply");
    let Some(RelayReply::HostInfo {
        addr: host,
        token,
        bind_key,
    }) = RelayReply::decode(&buf[..n]).expect("valid")
    else {
        panic!("expected host info");
    };
    assert_eq!((host, token.as_slice()), (addr(7001), &b"token"[..]));
    player
        .send_to(&encode_bind(host, &bind_key), relay_addr)
        .expect("send");
    relay.update(2);
    assert_eq!(relay.core().flow_count(), 1);
    // The game client talks to the relay as if it were the server.
    let mut a =
        NetClient::<Duel, _>::connect(player, relay_addr, ClientConfig::default(), &Team(0)).expect("fits");
    let mut now = 10;
    for _ in 0..200 {
        now += STEP_MS;
        a.update(now);
        relay.update(now);
        while let Some((n, from)) = host_leg.recv_from(&mut buf).expect("io") {
            let to = if from == relay_addr {
                addr(7777)
            } else {
                relay_addr
            };
            host_leg.send_to(&buf[..n], to).expect("forward");
        }
        s.update(now);
        if a.is_connected() {
            break;
        }
    }
    assert!(a.is_connected(), "joined through the relay");
    a.send(Channel::GAME_RPC, &DuelMsg::Chat("via relay".into()))
        .expect("queued");
    for _ in 0..20 {
        now += STEP_MS;
        a.update(now);
        relay.update(now);
        while let Some((n, from)) = host_leg.recv_from(&mut buf).expect("io") {
            let to = if from == relay_addr {
                addr(7777)
            } else {
                relay_addr
            };
            host_leg.send_to(&buf[..n], to).expect("forward");
        }
        s.update(now);
    }
    let chat = server_events(&mut s)
        .into_iter()
        .any(|e| matches!(e, ServerEvent::Message { message: DuelMsg::Chat(ref t), .. } if t == "via relay"));
    assert!(chat, "a reliable message crossed the relay");
    assert!(relay.core().forwarded > 0);
}

#[test]
fn agent_bridge_reports_and_applies_conditioner_changes() {
    let net = LoopbackNet::new();
    let mut s = server(&net, ServerConfig::default());
    let mut a = client(&net, "ada", 0, |_| {});
    let now = run(&mut s, &mut [&mut a], 0, 100, |_, cs| cs[0].is_connected());
    let mut state = NetAgentState::default();
    s.publish("server", &mut state);
    a.publish("client", &mut state);
    let server_status: &NetStatus = &state.reports()["server"].status;
    assert_eq!(server_status.peer_count, 1);
    assert_eq!(server_status.state, "listening");
    assert_eq!(state.reports()["server"].peers[0].name, "ada");
    assert_eq!(state.reports()["client"].status.state, "connected");
    let bad = ConditionerConfig::from_profile("bad").expect("profile");
    assert_eq!(
        state.request_conditioner(Some("server"), bad.clone()),
        vec!["server".to_string()]
    );
    s.publish("server", &mut state);
    a.publish("client", &mut state);
    assert_eq!(s.conditioner().config(), &bad.sanitized());
    assert!(!a.conditioner().config().enabled, "the client was not targeted");
    run(&mut s, &mut [&mut a], now, 50, |_, _| false);
    s.publish("server", &mut state);
    assert!(
        state.reports()["server"].status.conditioner_stats.delayed > 0,
        "the new profile is live"
    );
    let overlay = s.overlay();
    assert_eq!(overlay.peers.len(), 1);
    assert!(overlay.lines()[0].starts_with("server listening"));
}

#[test]
fn per_address_cap_keeps_one_machine_from_taking_every_slot() {
    let net = LoopbackNet::new();
    let mut s = server(
        &net,
        ServerConfig {
            max_peers_per_ip: 1,
            ..ServerConfig::default()
        },
    );
    let mut a = client(&net, "ada", 0, |_| {});
    let mut b = client(&net, "bob", 1, |_| {});
    run(&mut s, &mut [&mut a, &mut b], 0, 100, |_, _| false);
    assert!(a.is_connected());
    assert!(matches!(
        b.state(),
        ClientState::Ended(ClientEnd::Denied {
            reason: DenyReason::Full,
            ..
        })
    ));
    assert_eq!(s.session().len(), 1);
}

#[test]
fn an_accept_before_any_challenge_is_not_taken() {
    use regia_net::handshake::{Accept, ConnectRequest, PacketKind, split_datagram};
    let net = LoopbackNet::new();
    // Whoever answers on the server's address can read the nonce from the clear request.
    let mut forger = net.bind(addr(7777)).expect("bind forger");
    let mut c = client(&net, "victim", 0, |_| {});
    c.update(0);
    let mut buf = [0u8; 2048];
    let (n, from) = forger.recv_from(&mut buf).expect("reads").expect("a request");
    let (kind, body) = split_datagram(&buf[..n]).expect("a packet");
    assert_eq!(kind, PacketKind::ConnectRequest);
    let request = ConnectRequest::decode(body).expect("decodes");
    let forged = Accept {
        client_nonce: request.client_nonce,
        session_token: 0xdead_beef,
        peer_id: 1,
        tick_rate_hz: 60,
        server_tick: 0,
        resume_ticket: [0; 16],
        resumption: None,
    };
    forger.send_to(&forged.to_datagram(), from).expect("sends");
    c.update(STEP_MS);
    assert!(!c.is_connected());
    assert_eq!(c.totals().malformed, 1);
}
