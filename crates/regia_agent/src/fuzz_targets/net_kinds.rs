// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for every `regia_net` message kind the older `net.rs` targets left out (ADR-2084): one target per
//! session message kind, the connected packet body, the crypto datagrams, each relay request and reply, the NAT-PMP
//! responses, the server endpoint's whole datagram path, and the replication frames a client and a server read.
//!
//! Every fixture is encoded by `regia_net`'s own encoder or produced by its own code (a real relay, a real server
//! with a fixed secret, a loopback hub), so a fixture always matches the current wire layout.

use std::fmt::Display;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use regia_app::prelude::*;
use regia_net::connection::Connection;
use regia_net::handshake::{self, ChallengeResponse, JoinToken, PacketKind};
use regia_net::model::{NetMode, NetPlayerId};
use regia_net::relay::{self, RelayCore, RelayError, RelayReply, RelayTag};
use regia_net::session::manifest::{ContentManifest, ManifestMod};
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg, Welcome};
use regia_net::testing::LoopbackHub;
use regia_net::wire::{WireReader, WireWriter};
use regia_net::{
    BroadcastMessage, Direction, LinkConfig, NetMessage, NetServer, PeerId, Replicate, ReplicateOptions,
    Replicated, ServerConfig, ToServerMessage,
};

use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

const fn bytes(
    name: &'static str,
    round_trip: bool,
    fixture: fn() -> Vec<u8>,
    load: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format: None,
        format_version: regia_net::REGIA_NET_WIRE_VERSION,
        round_trip,
        input: FuzzInput::Bytes { fixture, load },
    }
}

// ---- Session messages (spec 77 § 4): one target per kind ------------------------------------------------------

/// Decode, then encode: the session layer reads every kind with the same entry point, and a per-kind fixture makes
/// the mutations land in that kind's body instead of mostly breaking the tag byte.
fn session_load(b: &[u8]) -> Result<Vec<u8>, String> {
    SessionMsg::decode(b)
        .and_then(|m| m.encode())
        .map_err(|err| format!("{err:?}"))
}

fn session(m: &SessionMsg) -> Vec<u8> {
    m.encode().unwrap_or_default()
}

fn manifest() -> ContentManifest {
    ContentManifest {
        format_version: regia_net::session::manifest::NET_MANIFEST_FORMAT_VERSION,
        engine_version: "0.2.10".into(),
        game_version: "1.4.0".into(),
        protocol_version: 3,
        protocol_hash: 0x1234_5678_9ABC_DEF0,
        content_hash: 0x0FED_CBA9_8765_4321,
        packs: vec![("base".into(), 11), ("maps".into(), 22)],
        mods: vec![ManifestMod {
            id: "example.lights".into(),
            version: "1.0.0".into(),
            hash: 33,
        }],
    }
}

fn hello_fixture() -> Vec<u8> {
    session(&SessionMsg::Hello(Box::new(Hello {
        protocol_version: 3,
        protocol_hash: 0x1234_5678_9ABC_DEF0,
        manifest: manifest(),
        player_names: vec!["Ada".into(), "Brin".into()],
        password: Some("hunter2".into()),
        auth: Some(vec![1, 2, 3, 4]),
        client_rate_bytes: 64_000,
        reconnect_token: Some([7; 16]),
    })))
}

fn welcome_fixture() -> Vec<u8> {
    session(&SessionMsg::Welcome(Box::new(Welcome {
        peer_id: PeerId(3),
        session_id: 0xABCD,
        server_tick: 1200,
        tick_hz: 60,
        send_every_ticks: 2,
        level: "levels/harbor.level.ron".into(),
        travel_id: 4,
        motd: "welcome".into(),
        server_name: "Fuzz host".into(),
        reconnect_token: [9; 16],
        players: vec![
            NetPlayerId {
                peer: PeerId(3),
                local: 0,
            },
            NetPlayerId {
                peer: PeerId(3),
                local: 1,
            },
        ],
    })))
}

/// The session targets, one per kind (`net.session_msg` keeps `LevelReady`).
fn session_targets() -> Vec<FuzzTarget> {
    let t = |name, fixture| bytes(name, true, fixture, session_load);
    vec![
        t("net.session.hello", hello_fixture),
        t("net.session.welcome", welcome_fixture),
        t("net.session.refused", || {
            session(&SessionMsg::Refused(DisconnectReason::VersionMismatch(
                "needs 1.4".into(),
            )))
        }),
        t("net.session.join_removed", || {
            session(&SessionMsg::JoinRemoved {
                guids: vec![1, u128::MAX, 0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEF],
            })
        }),
        t("net.session.join_records", || {
            session(&SessionMsg::JoinRecords {
                bytes: vec![5, 4, 3, 2, 1, 0],
            })
        }),
        t("net.session.in_game", || {
            session(&SessionMsg::InGame { tick: 88 })
        }),
        t("net.session.travel_begin", || {
            session(&SessionMsg::TravelBegin {
                level: "levels/yard.level.ron".into(),
                travel_id: 9,
                keep: Some(regia_net::session::TravelKeep {
                    below: regia_net::model::NetId(40),
                    ids: vec![regia_net::model::NetId(3), regia_net::model::NetId(17)],
                }),
            })
        }),
        t("net.session.goodbye", || {
            session(&SessionMsg::Goodbye(DisconnectReason::Kicked("afk".into())))
        }),
        t("net.session.keepalive", || session(&SessionMsg::Keepalive)),
        t("net.session.player_removed", || {
            session(&SessionMsg::PlayerRemoved {
                local: 1,
                reason: DisconnectReason::TooManyLocalPlayers,
            })
        }),
        t("net.session.stream_begin", || {
            session(&SessionMsg::StreamBegin {
                id: 2,
                kind: "replay".into(),
                total: 4096,
            })
        }),
        t("net.session.stream_chunk", || {
            session(&SessionMsg::StreamChunk {
                id: 2,
                bytes: vec![9; 24],
            })
        }),
        t("net.session.admin_login", || {
            session(&SessionMsg::AdminLogin {
                token: "admin-token-0123".into(),
            })
        }),
        t("net.session.admin_command", || {
            session(&SessionMsg::AdminCommand {
                line: "kick 3 griefing".into(),
            })
        }),
        t("net.session.admin_output", || {
            session(&SessionMsg::AdminOutput {
                lines: vec!["kicked 3".into(), "ok".into()],
            })
        }),
        t("net.session.game", || {
            session(&SessionMsg::Game {
                bytes: vec![1, 1, 2, 3, 5, 8, 13],
            })
        }),
    ]
}

// ---- Connected traffic and the crypto datagrams ---------------------------------------------------------------

/// A packet body with a reliable message split over two fragments and an unreliable one: what
/// `Connection::receive` reads after an endpoint strips its header.
fn payload_fixture() -> Vec<u8> {
    let config = LinkConfig {
        max_packet_bytes: 200,
        ..LinkConfig::default()
    };
    let mut c = Connection::new(config);
    let _ = c.send(regia_net::Channel::GAME_RPC, vec![0xAB; 300], 0);
    let _ = c.send(regia_net::Channel::Unreliable, vec![1, 2, 3], 0);
    let packets = c.poll(0, &[]);
    packets.into_iter().next().unwrap_or_default()
}

/// A fresh connection receives the body; the messages it delivers are the "encoding" (no writer re-creates a body
/// byte for byte: acks and sequence numbers belong to the sender).
fn payload_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut c = Connection::new(LinkConfig::default());
    c.receive(b, 10).map_err(e)?;
    // The same body again is a duplicate; it must be ignored, not double-delivered or failed.
    c.receive(b, 11).map_err(e)?;
    Ok(Vec::new())
}

/// The link of `net.payload_sequence`: small packets, so one message is many fragments, and a small out-of-order
/// cap, so a mutation can reach it.
fn sequence_config() -> LinkConfig {
    LinkConfig {
        max_packet_bytes: 200,
        connection_buffer_bytes: 4096,
        ..LinkConfig::default()
    }
}

/// Spec 120 § 5.2 (ADR-7981): one 3,000-byte reliable message in 17 fragments plus an unreliable message, the
/// packets in reverse order, so every fragment but the last waits ahead of a gap until the first arrives.
fn payload_sequence_fixture() -> Vec<u8> {
    let mut c = Connection::new(sequence_config());
    let message: Vec<u8> = (0..3000_u32).map(|i| (i % 251) as u8).collect();
    let _ = c.send(regia_net::Channel::GAME_RPC, message, 0);
    let _ = c.send(regia_net::Channel::Unreliable, vec![1, 2, 3], 0);
    let mut packets = c.poll(0, &[]);
    packets.reverse();
    frame(&packets)
}

/// A fresh connection receives each body 300 ms after the one before (a gap left open long enough fails as
/// `reassembly_stalled`); after every one the reassembly caps hold, and a failed connection holds nothing.
fn payload_sequence_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut c = Connection::new(sequence_config());
    for (i, body) in unframe(b)?.iter().enumerate() {
        let result = c.receive(body, 300 * i as u64);
        let stats = c.stats();
        assert!(
            stats.reliable_buffered_bytes <= c.reassembly_cap_bytes(),
            "{} B stored ahead of a gap, over the cap {}",
            stats.reliable_buffered_bytes,
            c.reassembly_cap_bytes()
        );
        assert!(
            stats.reliable_assembling_bytes <= c.config().max_reliable_message_bytes,
            "{} B assembling, over the message cap",
            stats.reliable_assembling_bytes
        );
        if let Err(err) = result {
            if let Some(failure) = c.failure() {
                assert_eq!(
                    (stats.reliable_buffered_bytes, stats.reliable_assembling_bytes),
                    (0, 0),
                    "a failed connection ({}) frees its reassembly",
                    failure.as_str()
                );
            }
            return Err(e(err));
        }
        while c.next_message().is_some() {}
    }
    Ok(Vec::new())
}

fn crypto_targets() -> Vec<FuzzTarget> {
    use regia_net::crypto::{KeyInit, KeyReply, SealedHeader};
    fn body(d: &[u8], kind: PacketKind) -> Result<&[u8], String> {
        match handshake::split_datagram(d) {
            Some((k, body)) if k == kind => Ok(body),
            Some((k, _)) => Err(format!("expected a {kind:?} datagram, found {k:?}")),
            None => Err("not a handshake datagram".into()),
        }
    }
    vec![
        bytes(
            "net.key_init",
            true,
            || {
                KeyInit {
                    client_nonce: 7,
                    cookie: [3; 16],
                    version: 1,
                    noise: [5; 32],
                }
                .to_datagram()
            },
            |d| {
                KeyInit::decode(body(d, PacketKind::KeyInit)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.key_reply",
            true,
            || {
                KeyReply {
                    client_nonce: 7,
                    noise: [6; 100],
                }
                .to_datagram()
            },
            |d| {
                KeyReply::decode(body(d, PacketKind::KeyReply)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.sealed_header",
            true,
            || {
                let mut out = [0u8; regia_net::crypto::SEALED_HEADER_BYTES];
                SealedHeader::write(&mut out, 0xDEAD_BEEF, 41);
                let mut v = out.to_vec();
                v.extend_from_slice(&[0x5A; 40]);
                v
            },
            |d| {
                let h = SealedHeader::parse(body(d, PacketKind::Sealed)?)
                    .ok_or("too short for a sealed record")?;
                let mut out = [0u8; regia_net::crypto::SEALED_HEADER_BYTES];
                SealedHeader::write(&mut out, h.token, h.counter);
                let mut v = out.to_vec();
                v.extend_from_slice(h.ciphertext);
                Ok(v)
            },
        ),
        // Wire 6 (ADR-8858): the one-round-trip resume datagrams.
        bytes("net.resume_init", true, resume_init_fixture, resume_init_load),
        bytes(
            "net.resume_reply",
            true,
            || {
                regia_net::crypto::ResumeReply {
                    client_nonce: 7,
                    noise: [6; regia_net::crypto::RESUME_REPLY_NOISE_BYTES],
                    accept: vec![0x5A; 64],
                }
                .to_datagram()
                .unwrap_or_default()
            },
            |d| {
                regia_net::crypto::ResumeReply::decode(body(d, PacketKind::ResumeReply)?)
                    .and_then(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.resume_retry",
            true,
            || {
                regia_net::crypto::ResumeRetry {
                    client_nonce: 7,
                    cookie: [3; 16],
                }
                .to_datagram()
            },
            |d| {
                regia_net::crypto::ResumeRetry::decode(body(d, PacketKind::ResumeRetry)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
    ]
}

/// The fixed ticket secret, period and PSK of the `net.resume_init` target, so its ticket repeats.
const RESUME_TICKET_SECRET: [u8; 32] = [0x42; 32];
const RESUME_TICKET_PERIOD_MS: u64 = 60_000;
const RESUME_PSK: [u8; 32] = [0x17; 32];

/// A `ResumeInit` whose ticket opens under the target's keys. Its Noise message is fixed bytes (a real one
/// has a random ephemeral key), so the fixture repeats; the load below still runs the ticket's open and the
/// first message's open on every input.
fn resume_init_fixture() -> Vec<u8> {
    use regia_net::crypto::{ResumeInit, TicketClaims, TicketKeys};
    let claims = TicketClaims {
        peer: 3,
        epoch: 1,
        psk: RESUME_PSK,
        block: regia_net::addr_block::AddrBlock::of(Ipv4Addr::LOCALHOST.into()).key_bytes(),
        issued_ms: 0,
        expires_ms: RESUME_TICKET_PERIOD_MS,
        protocol_id: 0x1234_5678_9ABC_DEF0,
        build_hash: regia_net::crypto::build_hash(""),
        password_verified: false,
        resume_id: [9; handshake::RESUME_TICKET_BYTES],
    };
    let ticket = TicketKeys::new(&RESUME_TICKET_SECRET, RESUME_TICKET_PERIOD_MS)
        .seal(&claims, 0)
        .unwrap_or([0; handshake::RESUMPTION_TICKET_BYTES]);
    ResumeInit {
        protocol_id: claims.protocol_id,
        client_nonce: 7,
        crypto_version: regia_net::crypto::CRYPTO_VERSION,
        cookie: Some([3; 16]),
        ticket,
        noise: vec![5; 96],
    }
    .to_datagram()
    .unwrap_or_default()
}

/// Decode, then what a server does with the fields before any state exists (the ticket's open, the first
/// Noise message's open under the ticket's PSK), then encode. The two opens may refuse; they must not panic.
fn resume_init_load(d: &[u8]) -> Result<Vec<u8>, String> {
    use regia_net::crypto::{ResumeInit, ResumeResponder, TicketKeys};
    let kind = PacketKind::ResumeInit;
    let body = match handshake::split_datagram(d) {
        Some((k, body)) if k == kind => body,
        Some((k, _)) => return Err(format!("expected a {kind:?} datagram, found {k:?}")),
        None => return Err("not a handshake datagram".into()),
    };
    let init = ResumeInit::decode(body).map_err(e)?;
    let _ = ResumeInit::stamp_of(body);
    let keys = TicketKeys::new(&RESUME_TICKET_SECRET, RESUME_TICKET_PERIOD_MS);
    let psk = keys.open(&init.ticket, 1).map_or(RESUME_PSK, |claims| claims.psk);
    let _ = ResumeResponder::open(
        &psk,
        init.protocol_id,
        init.client_nonce,
        init.crypto_version,
        &init.ticket,
        &init.noise,
    );
    init.to_datagram().map_err(e)
}

// ---- The server endpoint: every handshake kind and connected traffic through `handle_datagram` ----------------

/// The fixed secret of the fuzzed server, so its cookies and session tokens repeat.
const SERVER_SECRET: [u8; 32] = [0x42; 32];
/// Datagrams in one endpoint input at most (a mutated count must not loop for long).
const MAX_ENDPOINT_DATAGRAMS: usize = 64;

type FuzzServer = NetServer<regia_net::session::net_session::SessionWire, regia_net::LoopbackSocket>;

/// A plain server on loopback (no encryption, so the replayed handshake needs no key exchange).
fn endpoint_server(net: &regia_net::LoopbackNet) -> Result<(FuzzServer, SocketAddr), String> {
    let socket = net
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 7777)))
        .map_err(e)?;
    let config = ServerConfig {
        crypto: regia_net::CryptoMode::Off,
        max_handshakes_per_s: 0,
        handshakes_per_ip_per_s: 0,
        ..ServerConfig::default()
    };
    let server = FuzzServer::with_secret(socket, config, SERVER_SECRET);
    Ok((server, SocketAddr::from((Ipv4Addr::LOCALHOST, 7777))))
}

/// Frames datagrams as `u16 length, bytes` records.
fn frame(datagrams: &[Vec<u8>]) -> Vec<u8> {
    let mut w = WireWriter::new();
    for d in datagrams {
        let _ = w.bytes_u16(d, usize::from(u16::MAX));
    }
    w.finish()
}

fn unframe(b: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut r = WireReader::new(b);
    let mut out = Vec::new();
    while !r.is_empty() {
        if out.len() == MAX_ENDPOINT_DATAGRAMS {
            return Err(format!("more than {MAX_ENDPOINT_DATAGRAMS} datagrams"));
        }
        out.push(r.bytes_u16(usize::from(u16::MAX)).map_err(e)?.to_vec());
    }
    if out.is_empty() {
        return Err("no datagram".into());
    }
    Ok(out)
}

/// A real client's handshake against the fixed-secret server, then connected traffic: a connect request, the
/// challenge response with the server's cookie, a payload packet with two messages, a disconnect, and a key init.
fn endpoint_fixture() -> Vec<u8> {
    endpoint_script().unwrap_or_default()
}

fn endpoint_script() -> Result<Vec<u8>, String> {
    use regia_net::NetCodec;
    let net = regia_net::LoopbackNet::new();
    let (mut server, server_addr) = endpoint_server(&net)?;
    let mut client = net
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 40_001)))
        .map_err(e)?;
    let protocol_id = regia_net::protocol_id::<regia_net::session::net_session::SessionWire>();
    let request = handshake::ConnectRequest {
        protocol_id,
        client_nonce: 0x5151,
        crypto_version: regia_net::crypto::PLAIN_VERSION,
        crypto_required: false,
    }
    .to_datagram();
    let reply = exchange(&mut server, &mut client, server_addr, &request, 10)?;
    let challenge = handshake::Challenge::decode(reply_body(&reply, PacketKind::Challenge)?).map_err(e)?;
    let response = ChallengeResponse {
        client_nonce: 0x5151,
        cookie: challenge.cookie,
        token: JoinToken {
            name: "fuzzer".into(),
            role: regia_net::PeerRole::Player,
            build: String::new(),
            password_proof: [0; 32],
            join_data: ().to_bytes().map_err(e)?,
            resume: None,
        },
    }
    .to_datagram()
    .map_err(e)?;
    let reply = exchange(&mut server, &mut client, server_addr, &response, 20)?;
    let accept = handshake::Accept::decode(reply_body(&reply, PacketKind::Accept)?).map_err(e)?;
    let mut link = Connection::new(LinkConfig::default());
    let _ = link.send(
        regia_net::Channel::GAME_RPC,
        vec![1u8, 2, 3].to_bytes().map_err(e)?,
        0,
    );
    let _ = link.send(
        regia_net::Channel::Unreliable,
        vec![4u8].to_bytes().map_err(e)?,
        0,
    );
    let header = handshake::connected_header(PacketKind::Payload, accept.session_token);
    let mut datagrams = vec![request, response];
    datagrams.extend(link.poll(30, &header));
    datagrams.push(handshake::connected_header(PacketKind::Disconnect, accept.session_token).to_vec());
    datagrams.push(
        regia_net::crypto::KeyInit {
            client_nonce: 0x5151,
            cookie: challenge.cookie,
            version: 1,
            noise: [1; 32],
        }
        .to_datagram(),
    );
    Ok(frame(&datagrams))
}

fn reply_body(d: &[u8], kind: PacketKind) -> Result<&[u8], String> {
    match handshake::split_datagram(d) {
        Some((k, body)) if k == kind => Ok(body),
        other => Err(format!("expected {kind:?}, got {:?}", other.map(|(k, _)| k))),
    }
}

/// Sends `datagram` from `client`, lets the server read it, and returns the server's first reply.
fn exchange(
    server: &mut FuzzServer,
    client: &mut regia_net::LoopbackSocket,
    server_addr: SocketAddr,
    datagram: &[u8],
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    use regia_net::DatagramSocket;
    client.send_to(datagram, server_addr).map_err(e)?;
    server.receive(now_ms);
    server.flush(now_ms);
    let mut buf = vec![0u8; 2048];
    match client.recv_from(&mut buf).map_err(e)? {
        Some((n, _)) => {
            buf.truncate(n);
            Ok(buf)
        }
        None => Err("the server did not answer".into()),
    }
}

/// Replays the framed datagrams from the client address into a fresh fixed-secret server, one receive and flush per
/// datagram, then drains its events. The server answers or drops; it never fails, so the target is a handler.
fn endpoint_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_net::DatagramSocket;
    let datagrams = unframe(b)?;
    let net = regia_net::LoopbackNet::new();
    let (mut server, server_addr) = endpoint_server(&net)?;
    let mut client = net
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 40_001)))
        .map_err(e)?;
    let mut now = 10;
    for d in &datagrams {
        client.send_to(d, server_addr).map_err(e)?;
        server.receive(now);
        server.flush(now);
        now += 10;
    }
    while server.poll_event().is_some() {}
    Ok(Vec::new())
}

// ---- Relay requests (server side) and replies (client side) ---------------------------------------------------

fn relay_from() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 50000))
}

fn relay_host() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, 5), 40000))
}

/// A relay with one registered host (the register handshake answered with its cookie), so lookups, binds and
/// introductions reach their handlers instead of the "not registered" refusal.
fn relay_with_host() -> RelayCore {
    relay_with_host_code().0
}

/// [`relay_with_host`] and the join code its host got.
fn relay_with_host_code() -> (RelayCore, Option<String>) {
    let mut code = None;
    let mut relay = RelayCore::with_secret(5, [0x24; 32]);
    // Protocol 2 allocation ports, so an `Allocate` reaches its handler instead of `Full { cap: 0 }`.
    relay.add_allocation_ports(RELAY_FUZZ_PORT..RELAY_FUZZ_PORT + 4);
    let first = relay.handle(
        relay_host(),
        &relay::encode_register(600, b"host-token").unwrap_or_default(),
        1000,
    );
    if let Some(RelayReply::RegisterChallenge(cookie)) = first
        .first()
        .and_then(|(d, _)| RelayReply::decode(d).ok().flatten())
        && let Ok(again) = relay::encode_register_with_cookie(600, b"host-token", &cookie)
    {
        for (d, _) in relay.handle(relay_host(), &again, 1001) {
            if let Ok(Some(RelayReply::JoinCode(c))) = RelayReply::decode(&d) {
                code = Some(c);
            }
        }
    }
    (relay, code)
}

/// First allocation port of the fuzzed relay.
const RELAY_FUZZ_PORT: u16 = 50_000;
/// The fuzzed client's allocation key.
const RELAY_FUZZ_CLIENT_KEY: [u8; 16] = [0x31; 16];

/// The fuzzed relay's bind key for its host (what `Lookup` hands out).
fn relay_bind_key(relay: &mut RelayCore, code: Option<String>) -> Option<[u8; 16]> {
    let lookup = relay::encode_lookup(&code?)?;
    match relay
        .handle(relay_from(), &lookup, 1003)
        .first()
        .and_then(|(d, _)| RelayReply::decode(d).ok().flatten())
    {
        Some(RelayReply::HostInfo { bind_key, .. }) => Some(bind_key),
        _ => None,
    }
}

/// The relay with one allocation held by [`relay_from`] (the cookie round trip answered); its port.
fn relay_with_allocation() -> (RelayCore, u16) {
    let (mut relay, code) = relay_with_host_code();
    let Some(key) = relay_bind_key(&mut relay, code) else {
        return (relay, RELAY_FUZZ_PORT);
    };
    let ask = relay::encode_allocate(relay_host(), &key, &RELAY_FUZZ_CLIENT_KEY, None);
    let first = relay.handle(relay_from(), &ask, 1004);
    if let Some(RelayReply::Challenge(cookie)) = first
        .first()
        .and_then(|(d, _)| RelayReply::decode(d).ok().flatten())
    {
        let again = relay::encode_allocate(relay_host(), &key, &RELAY_FUZZ_CLIENT_KEY, Some(&cookie));
        for (d, _) in relay.handle(relay_from(), &again, 1005) {
            if let Ok(Some(RelayReply::Allocated { port, .. })) = RelayReply::decode(&d) {
                return (relay, port);
            }
        }
    }
    (relay, RELAY_FUZZ_PORT)
}

/// The relay's allocation-port handler (a `Refresh`, or bytes to forward) from the allocation's
/// client and from a stranger.
fn relay_allocation_load(d: &[u8]) -> Result<Vec<u8>, String> {
    let (mut relay, port) = relay_with_allocation();
    let replies = relay.handle_allocation(port, relay_from(), d, 2000);
    let stranger = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 99), 1));
    let again = relay.handle_allocation(port, stranger, d, 2001);
    if replies.is_empty() && again.is_empty() {
        return Err("dropped".into());
    }
    Ok(Vec::new())
}

/// The relay's datagram handler with a registered host; the replies are not an encoding (no writer).
fn relay_request_load(d: &[u8]) -> Result<Vec<u8>, String> {
    let mut relay = relay_with_host();
    let replies = relay.handle(relay_from(), d, 2000);
    let again = relay.handle(relay_from(), d, 2001);
    if replies.is_empty() && again.is_empty() {
        return Err("dropped".into());
    }
    Ok(Vec::new())
}

fn relay_request_targets() -> Vec<FuzzTarget> {
    let t = |name, fixture| bytes(name, false, fixture, relay_request_load);
    vec![
        t("net.relay.lookup", || {
            relay::encode_lookup(&regia_net::join_code::mint_join_code(77)).unwrap_or_default()
        }),
        t(
            "net.relay.punch_intro",
            || relay::encode_punch_intro(relay_host()),
        ),
        t("net.relay.bind", || relay::encode_bind(relay_host(), &[8; 16])),
        t("net.relay.publish", || {
            relay::encode_publish(b"listing: harbor 3/8").unwrap_or_default()
        }),
        t("net.relay.list_games", || relay::encode_list_games_page(0)),
        // Relay protocol 2 (spec 120 § 10.6, ADR-7982).
        t("net.relay.allocate", || {
            relay::encode_allocate(relay_host(), &[8; 16], &RELAY_FUZZ_CLIENT_KEY, Some(&[4; 16]))
        }),
        bytes(
            "net.relay.refresh",
            false,
            || relay::encode_refresh(&RELAY_FUZZ_CLIENT_KEY, Some(&[4; 16])),
            relay_allocation_load,
        ),
    ]
}

/// The client's reply decoder, then the reply encoded again with the relay's layout.
fn relay_reply_load(d: &[u8]) -> Result<Vec<u8>, String> {
    match RelayReply::decode(d).map_err(e)? {
        Some(reply) => encode_reply(&reply),
        None => Err("not relay control traffic".into()),
    }
}

/// The relay's reply layout (`RelayCore` writes these; no public writer exists for a decoded reply).
fn encode_reply(reply: &RelayReply) -> Result<Vec<u8>, String> {
    let mut w = WireWriter::new();
    w.raw(&relay::RELAY_MAGIC);
    // The protocol 2 replies travel in the versioned framing; `VersionTooOld` answers a protocol 1
    // peer in its own framing.
    let versioned = matches!(
        reply,
        RelayReply::Allocated { .. }
            | RelayReply::ClientJoined { .. }
            | RelayReply::Challenge(_)
            | RelayReply::Full { .. }
            | RelayReply::VersionTooNew { .. }
            | RelayReply::Introduced(_)
            | RelayReply::ProbeSent { .. }
            | RelayReply::ProbeHello { .. }
            | RelayReply::MeasureAck { .. }
    );
    if versioned {
        w.u8(relay::RELAY_VERSIONED);
        w.u8(relay::RELAY_PROTOCOL_VERSION);
    }
    match reply {
        RelayReply::JoinCode(code) => {
            w.u8(RelayTag::JoinCode as u8);
            w.raw(code.as_bytes());
        }
        RelayReply::RegisterChallenge(cookie) => {
            w.u8(RelayTag::RegisterChallenge as u8);
            w.raw(cookie);
        }
        RelayReply::HostInfo {
            addr,
            token,
            bind_key,
        } => {
            w.u8(RelayTag::HostInfo as u8);
            handshake::encode_addr(&mut w, *addr);
            w.bytes_u16(token, relay::RELAY_TOKEN_MAX_BYTES).map_err(e)?;
            w.raw(bind_key);
        }
        RelayReply::PunchPeer(addr) => {
            w.u8(RelayTag::PunchPeer as u8);
            handshake::encode_addr(&mut w, *addr);
        }
        RelayReply::Directory {
            page,
            pages,
            listings,
        } => {
            w.u8(RelayTag::Directory as u8);
            w.u8(*page);
            w.u8(*pages);
            w.u8(u8::try_from(listings.len()).map_err(|_| "too many listings".to_string())?);
            for l in listings {
                w.bytes_u16(l, relay::RELAY_LISTING_MAX_BYTES).map_err(e)?;
            }
        }
        RelayReply::Error(code) => {
            w.u8(RelayTag::Error as u8);
            w.u8(*code as u8);
        }
        RelayReply::Allocated { port, peer } => {
            w.u8(RelayTag::Allocated as u8);
            w.u16(*port);
            handshake::encode_addr(&mut w, *peer);
        }
        RelayReply::ClientJoined {
            port,
            client,
            host_key,
        } => {
            w.u8(RelayTag::ClientJoined as u8);
            w.u16(*port);
            handshake::encode_addr(&mut w, *client);
            w.raw(host_key);
        }
        RelayReply::Challenge(cookie) => {
            w.u8(RelayTag::Challenge as u8);
            w.raw(cookie);
        }
        RelayReply::Full { cap } => {
            w.u8(RelayTag::AllocationFull as u8);
            w.u16(*cap);
        }
        RelayReply::VersionTooOld { min } => {
            w.u8(RelayTag::VersionTooOld as u8);
            w.u8(*min);
        }
        RelayReply::VersionTooNew { max } => {
            w.u8(RelayTag::VersionTooNew as u8);
            w.u8(*max);
        }
        // Protocol 3 (ADR-8340): `PunchPeer` in the protocol 3 framing is an introduction.
        RelayReply::Introduced(intro) => {
            w.u8(RelayTag::PunchPeer as u8);
            handshake::encode_addr(&mut w, intro.peer);
            w.bytes_u16(&intro.token, relay::RELAY_TOKEN_MAX_BYTES)
                .map_err(e)?;
            w.raw(&intro.bind_key);
            w.raw(&intro.session);
            handshake::encode_addr(&mut w, intro.observed);
            regia_net::nat::encode_candidates(&mut w, &intro.candidates);
        }
        // Protocol 3 host check (ADR-8370).
        RelayReply::ProbeSent { nonce, sent } => {
            w.u8(RelayTag::ProbeSent as u8);
            w.raw(nonce);
            w.u8(u8::from(*sent));
        }
        RelayReply::ProbeHello { nonce } => {
            w.u8(RelayTag::ProbeHello as u8);
            w.raw(nonce);
        }
        RelayReply::MeasureAck { seq, capped } => {
            w.u8(RelayTag::MeasureAck as u8);
            w.u32(*seq);
            w.u8(u8::from(*capped));
        }
    }
    Ok(w.finish())
}

fn reply_fixture(reply: &RelayReply) -> Vec<u8> {
    encode_reply(reply).unwrap_or_default()
}

fn relay_reply_targets() -> Vec<FuzzTarget> {
    let t = |name, fixture| bytes(name, true, fixture, relay_reply_load);
    vec![
        t("net.relay_reply.join_code", || {
            reply_fixture(&RelayReply::JoinCode(regia_net::join_code::mint_join_code(77)))
        }),
        t("net.relay_reply.register_challenge", || {
            reply_fixture(&RelayReply::RegisterChallenge([4; 16]))
        }),
        t("net.relay_reply.host_info", || {
            reply_fixture(&RelayReply::HostInfo {
                addr: relay_host(),
                token: b"host-token".to_vec(),
                bind_key: [8; 16],
            })
        }),
        t("net.relay_reply.punch_peer", || {
            reply_fixture(&RelayReply::PunchPeer(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 0, 2, 1),
                9000,
            ))))
        }),
        t("net.relay_reply.directory", || {
            reply_fixture(&RelayReply::Directory {
                page: 0,
                pages: 2,
                listings: vec![b"harbor 3/8".to_vec(), b"yard 1/4".to_vec()],
            })
        }),
        t("net.relay_reply.error", || {
            reply_fixture(&RelayReply::Error(RelayError::Busy))
        }),
        t("net.relay_reply.allocated", || {
            reply_fixture(&RelayReply::Allocated {
                port: RELAY_FUZZ_PORT,
                peer: relay_host(),
            })
        }),
        t("net.relay_reply.client_joined", || {
            reply_fixture(&RelayReply::ClientJoined {
                port: RELAY_FUZZ_PORT,
                client: relay_from(),
                host_key: [6; 16],
            })
        }),
        t("net.relay_reply.challenge", || {
            reply_fixture(&RelayReply::Challenge([5; 16]))
        }),
        t("net.relay_reply.full", || {
            reply_fixture(&RelayReply::Full { cap: 40 })
        }),
        t("net.relay_reply.version_too_old", || {
            reply_fixture(&RelayReply::VersionTooOld { min: 2 })
        }),
        t("net.relay_reply.version_too_new", || {
            reply_fixture(&RelayReply::VersionTooNew { max: 2 })
        }),
        // Relay protocol 3 (spec 120 § 10.7, ADR-8340): both sides' candidates and the session key.
        t("net.relay_reply.introduced", || {
            use regia_net::nat::{Candidate, CandidateKind};
            reply_fixture(&RelayReply::Introduced(relay::Introduction {
                peer: relay_host(),
                token: b"RGJT ticket bytes".to_vec(),
                bind_key: [7; 16],
                session: [8; 16],
                observed: relay_from(),
                candidates: vec![
                    Candidate::new(CandidateKind::Host, relay_from()),
                    Candidate::new(CandidateKind::ServerReflexive, relay_host()),
                ],
            }))
        }),
        // A connectivity check from the game socket (ADR-8340): decode, then encode.
        bytes(
            "net.nat_check",
            true,
            || {
                regia_net::nat::encode_check(&regia_net::nat::Check::Request {
                    session: [9; 16],
                    txid: 77,
                    nominate: true,
                    priority: 1 << 30,
                })
            },
            |d| {
                regia_net::nat::decode_check(d)
                    .map(|c| regia_net::nat::encode_check(&c))
                    .ok_or_else(|| "not a check".to_string())
            },
        ),
    ]
}

// ---- NAT-PMP responses (RFC 6886) -----------------------------------------------------------------------------

fn nat_pmp_targets() -> Vec<FuzzTarget> {
    use regia_net::port_map::{
        nat_pmp_decode_address, nat_pmp_decode_map, nat_pmp_encode_address, nat_pmp_encode_map,
    };
    vec![
        bytes(
            "net.nat_pmp_address",
            true,
            || nat_pmp_encode_address(Ipv4Addr::new(203, 0, 113, 9), 12).to_vec(),
            |d| {
                nat_pmp_decode_address(d)
                    .map(|ip| nat_pmp_encode_address(ip, 0).to_vec())
                    .map_err(e)
            },
        ),
        bytes(
            "net.nat_pmp_map",
            true,
            || nat_pmp_encode_map(0, 7777, 40000, 7200).to_vec(),
            |d| {
                nat_pmp_decode_map(d)
                    .map(|(internal, external, life)| {
                        nat_pmp_encode_map(0, internal, external, life).to_vec()
                    })
                    .map_err(e)
            },
        ),
    ]
}

// ---- Replication frames (spec 70/72): what `net_receive` and `net_apply` read ---------------------------------

/// A replicated component with a number, a string and a list, so state frames carry every codec kind.
#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct FuzzCrate {
    hp: u32,
    label: String,
    marks: Vec<u16>,
}

/// A client → server message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct FuzzFire {
    power: u32,
}
impl NetMessage for FuzzFire {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for FuzzFire {}

/// A server → everyone message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct FuzzBoom {
    at: u32,
}
impl NetMessage for FuzzBoom {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for FuzzBoom {}

/// A server → everyone message on the unordered class: its own `RUMSG` frame (wire 6, ADR-10052 (3)).
#[derive(Clone, Debug, PartialEq, Replicate)]
struct FuzzWhisper {
    n: u32,
}
impl NetMessage for FuzzWhisper {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: regia_net::message::Channel = regia_net::message::Channel::ReliableUnordered;
}
impl BroadcastMessage for FuzzWhisper {}

/// A client → server message on the unordered class (a client `RUMSG` frame).
#[derive(Clone, Debug, PartialEq, Replicate)]
struct FuzzReport {
    n: u32,
}
impl NetMessage for FuzzReport {
    const DIRECTION: Direction = Direction::ToServer;
    const CHANNEL: regia_net::message::Channel = regia_net::message::Channel::ReliableUnordered;
}
impl ToServerMessage for FuzzReport {}

fn frame_setup(world: &mut World) {
    regia_net::message::register_net_message::<FuzzFire>(world);
    regia_net::message::register_net_message::<FuzzBoom>(world);
    regia_net::message::register_net_message::<FuzzWhisper>(world);
    regia_net::message::register_net_message::<FuzzReport>(world);
    regia_net::replication::register_replicated::<FuzzCrate>(world, ReplicateOptions::default());
}

/// A server and one client after `ticks` steps with two replicated crates.
fn frame_hub(ticks: usize) -> LoopbackHub {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, frame_setup);
    hub.add_client();
    for (i, label) in ["north", "south"].into_iter().enumerate() {
        hub.server.world.spawn((
            Replicated::default(),
            FuzzCrate {
                hp: 100 + i as u32,
                label: label.into(),
                marks: vec![1, 2, 3],
            },
        ));
    }
    hub.run(ticks);
    hub
}

/// Every frame the server sent the client in the first ticks (spawns, state, acks, an unordered
/// message's `RUMSG` frame), framed.
fn frame_to_client_fixture() -> Vec<u8> {
    let mut hub = frame_hub(3);
    let _ = regia_ecs::ecs::system::RunSystemOnce::run_system_once(
        &mut hub.server.world,
        |mut sender: regia_net::NetSender| {
            let _ = sender.broadcast_session(FuzzWhisper { n: 5 });
        },
    );
    hub.run(1);
    let frames: Vec<Vec<u8>> = hub
        .client_logs
        .first()
        .map(|log| log.iter().map(|(_, d)| d.bytes.clone()).collect())
        .unwrap_or_default();
    frame(&frames)
}

/// A fresh client reads the framed server frames, one tick each. A frame it cannot read closes the link
/// (`ProtocolError`): that is the rejection.
fn frame_to_client_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let frames = unframe(b)?;
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, frame_setup);
    let c = hub.add_client();
    let client = &mut hub.clients[c];
    for bytes in frames {
        if let Some(mut node) = client.world.get_resource_mut::<regia_net::runtime::NetNode>() {
            node.inbox.push(regia_net::runtime::Datagram {
                peer: PeerId::SERVER,
                channel: regia_net::message::Channel::ReliableOrdered,
                bytes,
            });
        }
        client.tick();
        if let Some(reason) = client.world.resource::<regia_net::runtime::NetNode>().closed {
            return Err(format!("the client closed the link: {reason:?}"));
        }
    }
    Ok(Vec::new())
}

/// What a client sends the server: a fire message and its acks, after a few ticks.
fn frame_to_server_fixture() -> Vec<u8> {
    let mut hub = frame_hub(2);
    if let Some(client) = hub.clients.first_mut() {
        let _ = regia_ecs::ecs::system::RunSystemOnce::run_system_once(
            &mut client.world,
            |mut sender: regia_net::NetSender| {
                let _ = sender.send_session_to_server(FuzzFire { power: 7 });
                let _ = sender.send_session_to_server(FuzzReport { n: 3 });
            },
        );
        client.tick();
        let out = std::mem::take(&mut client.world.resource_mut::<regia_net::runtime::NetNode>().outbox);
        return frame(&out.into_iter().map(|d| d.bytes).collect::<Vec<_>>());
    }
    Vec::new()
}

/// A fresh server with the client's peer reads the framed client frames, one tick each. A frame it cannot read
/// kicks the peer (`MalformedPacket`): that is the rejection.
fn frame_to_server_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let frames = unframe(b)?;
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, frame_setup);
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    for bytes in frames {
        if let Some(mut node) = hub.server.world.get_resource_mut::<regia_net::runtime::NetNode>() {
            node.inbox.push(regia_net::runtime::Datagram {
                peer,
                channel: regia_net::message::Channel::ReliableOrdered,
                bytes,
            });
        }
        hub.server.tick();
        let node = hub.server.world.resource::<regia_net::runtime::NetNode>();
        if let Some(d) = node.disconnects.iter().find(|d| d.peer == peer) {
            return Err(format!("the server kicked the peer: {:?}", d.reason));
        }
    }
    Ok(Vec::new())
}

// ---- Destruction over the network (ADR-1967, ADR-2305) -----------------------------------------------------------

/// A stream batch with every op kind (the relevance gain in two pieces and a skip included) and a snapshot part, as
/// one `u16`-length-prefixed list of encoded messages.
fn destruction_fixture() -> Vec<u8> {
    use regia_framework::destruction_net::{
        DestructionMessage, SnapshotPart, StreamBatch, WireInstance, WireOp,
    };
    use regia_net::bits::BitWriter;
    use regia_net::codec::{CodecContext, ReplicateCodec};
    use regia_physics::destruction::{
        BreakKind, CompactBreak, DestructibleId, DestructiblePose, DestructionSnapshot, DetachCause,
        InstanceSnapshot,
    };
    let id = DestructibleId(3);
    let pose = DestructiblePose::at([1.0, 2.0, 3.0]);
    let instance = |detached: Vec<u32>, broken: Vec<u32>| WireInstance {
        asset: 0xA55E7,
        state: InstanceSnapshot {
            id,
            pose,
            seed: 9,
            detached,
            broken,
        },
    };
    let messages = [
        DestructionMessage::Stream(StreamBatch {
            first_seq: 40,
            base_tick: 100,
            ops: vec![
                WireOp::Define {
                    id,
                    asset: 0xA55E7,
                    pose,
                    seed: 9,
                },
                WireOp::Compact {
                    id,
                    tick: 100,
                    kind: CompactBreak::Bond { bond: 7 },
                },
                WireOp::Compact {
                    id,
                    tick: 101,
                    kind: CompactBreak::Detach {
                        chunk: 2,
                        cause: DetachCause::Collapse,
                        linvel: [0.5, -1.0, 0.25],
                    },
                },
                WireOp::Full {
                    id,
                    tick: 102,
                    seed: 11,
                    kind: BreakKind::ChunkDetached {
                        chunk: 4,
                        cause: DetachCause::Damage,
                        position: [1.0, 2.0, 3.0],
                        linvel: [0.0, 1.0, 0.0],
                        angvel: [0.1, 0.2, 0.3],
                    },
                },
                WireOp::Skip {
                    next_seq: 60,
                    digest: 0xD16E57,
                },
                WireOp::Instance {
                    instance: instance(vec![1, 2], vec![5]),
                    more: true,
                },
                WireOp::Instance {
                    instance: instance(vec![], vec![9, 12]),
                    more: false,
                },
                WireOp::Check { hash: 0x4A5B },
                WireOp::Remove { id },
            ],
        }),
        DestructionMessage::Snapshot(SnapshotPart {
            sync: 2,
            index: 0,
            total: 1,
            header: DestructionSnapshot {
                next_id: 4,
                next_seq: 60,
                event_digest: 0xD16E57,
                tick: 103,
                instances: Vec::new(),
            },
            instances: vec![instance(vec![1, 2, 4], vec![5, 9, 12])],
        }),
    ];
    let cx = CodecContext::detached();
    let mut out = Vec::new();
    for m in &messages {
        let mut w = BitWriter::new();
        m.encode(&mut w, &cx);
        let bytes = w.finish();
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

/// Every message of the list decoded and encoded again; a message that does not decode, or an
/// empty list, is a rejection.
fn destruction_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_framework::destruction_net::DestructionMessage;
    use regia_net::bits::{BitReader, BitWriter};
    use regia_net::codec::{CodecContext, ReplicateCodec};
    // An empty list carries no message: the harness expects empty input to be rejected.
    if b.is_empty() {
        return Err("no message in the list".into());
    }
    let cx = CodecContext::detached();
    let mut rest = b;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let Some((len, tail)) = rest.split_first_chunk::<2>() else {
            return Err("a length prefix is cut short".into());
        };
        let len = usize::from(u16::from_le_bytes(*len));
        let Some((body, tail)) = tail.split_at_checked(len) else {
            return Err("a message is cut short".into());
        };
        let mut r = BitReader::new(body);
        let m = DestructionMessage::decode(&mut r, &cx).map_err(|err| format!("{err:?}"))?;
        let mut w = BitWriter::new();
        m.encode(&mut w, &cx);
        let bytes = w.finish();
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&bytes);
        rest = tail;
    }
    Ok(out)
}

/// The targets of this module.
pub(super) fn targets() -> Vec<FuzzTarget> {
    let mut all = session_targets();
    all.push(bytes("net.payload", false, payload_fixture, payload_load));
    all.push(bytes(
        "net.payload_sequence",
        false,
        payload_sequence_fixture,
        payload_sequence_load,
    ));
    all.extend(crypto_targets());
    all.push(bytes(
        "net.server_endpoint",
        false,
        endpoint_fixture,
        endpoint_load,
    ));
    all.extend(relay_request_targets());
    all.extend(relay_reply_targets());
    all.extend(nat_pmp_targets());
    all.push(bytes(
        "net.frame_to_client",
        false,
        frame_to_client_fixture,
        frame_to_client_load,
    ));
    all.push(bytes(
        "net.frame_to_server",
        false,
        frame_to_server_fixture,
        frame_to_server_load,
    ));
    all.push(bytes(
        "net.destruction_message",
        false,
        destruction_fixture,
        destruction_load,
    ));
    all.push(bytes(
        "net.anim_state",
        false,
        anim_state_fixture,
        anim_state_load,
    ));
    all
}

/// A `NetAnimState` (ADR-2551) of the demo net character, mid-walk with its upper-body layer
/// easing: the snapshot words of a real run of the animator and the graph's declared parameters.
fn anim_state_fixture() -> Vec<u8> {
    use regia_animation::pose_eval::ParamLayout;
    use regia_animation::pose_eval::STATE_WORDS_VERSION;
    use regia_animation::pose_eval::demo::demo_net_graph;
    use regia_framework::net_character::NetAnimState;
    use regia_net::bits::BitWriter;
    use regia_net::codec::{CodecContext, ReplicateCodec};
    // [version, graph, machine in Locomotion (entered), blend space slots (4 x [time, started]),
    // the jump clip, the layer clip, the layer's weight easing (13 words)].
    let mut words = vec![STATE_WORDS_VERSION, 0, 0, 1, 4];
    for t in [0.3f32, 0.3, 0.3, 0.3] {
        words.extend([t.to_bits(), 2]);
    }
    words.extend([0, 0, 0.15f32.to_bits(), 2]);
    words.extend([0u32; 13]);
    let mut set = regia_animation::params::ParameterSet::new();
    set.set_float(regia_animation::pose_eval::demo::SPEED, 1.0);
    set.set_float(regia_animation::pose_eval::demo::AIM, 1.0);
    let value = NetAnimState {
        tick: 120,
        words,
        params: ParamLayout::declared(&demo_net_graph()).encode(&set),
        blend_mask: 0b11,
        past: 0.25,
    };
    let mut w = BitWriter::new();
    value.encode(&mut w, &CodecContext::detached());
    w.finish()
}

/// Decodes a `NetAnimState`, restores its words into the demo character's animator and
/// evaluates the pose (a malformed state is refused, never a panic), then encodes it again.
fn anim_state_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_animation::components::Animator;
    use regia_animation::pose::Pose;
    use regia_animation::pose_eval::demo::demo_net_character;
    use regia_animation::pose_eval::{
        AnimStateSnapshot, ParamLayout, PoseRequest, evaluate_pose, restore_state,
    };
    use regia_framework::net_character::NetAnimState;
    use regia_net::bits::{BitReader, BitWriter};
    use regia_net::codec::{CodecContext, ReplicateCodec};
    let cx = CodecContext::detached();
    let mut r = BitReader::new(b);
    let value = NetAnimState::decode(&mut r, &cx).map_err(|err| format!("{err:?}"))?;
    let (skeleton, graph, clips) = demo_net_character().ok_or("no demo rig")?;
    let params = ParamLayout::declared(&graph).decode(&value.params);
    let mut animator = Animator::graph(graph, clips);
    let snapshot = AnimStateSnapshot::from_words(&value.words);
    restore_state(&mut animator, &snapshot).map_err(|err| err.to_string())?;
    let request = PoseRequest {
        skeleton: &skeleton,
        source: &animator.source,
        state: &snapshot,
        params: &params,
        delta: if value.past.is_finite() {
            value.past.clamp(0.0, 4.0) / 60.0
        } else {
            0.0
        },
        rig: Entity::PLACEHOLDER,
        socket_targets: None,
    };
    let mut pose = Pose::default();
    evaluate_pose(&request, &mut pose).map_err(|err| err.to_string())?;
    let mut w = BitWriter::new();
    value.encode(&mut w, &cx);
    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-2551: the anim-state fixture decodes, restores and evaluates; cut inputs are refused,
    /// never a panic.
    #[test]
    fn the_anim_state_fixture_loads_and_cut_inputs_are_refused() {
        let fixture = anim_state_fixture();
        assert_eq!(anim_state_load(&fixture), Ok(fixture.clone()));
        for n in 0..fixture.len() {
            let _ = anim_state_load(&fixture[..n]);
        }
        assert!(anim_state_load(&fixture[..4]).is_err());
    }

    /// The destruction fixture decodes whole and encodes back to the same bytes; a cut one is refused, never a panic.
    #[test]
    fn the_destruction_fixture_round_trips_and_cut_inputs_are_refused() {
        let fixture = destruction_fixture();
        assert_eq!(destruction_load(&fixture), Ok(fixture.clone()));
        for n in 0..fixture.len() {
            let _ = destruction_load(&fixture[..n]);
        }
        assert!(destruction_load(&fixture[..3]).is_err());
    }

    /// Every `SessionMsg` kind has a target here or in `net.session_msg` (the kind name decides).
    #[test]
    fn every_session_kind_has_a_target() {
        let mut kinds: Vec<&str> = session_targets()
            .iter()
            .map(|t| match t.input {
                FuzzInput::Bytes { fixture, .. } => regia_net::session::proto::kind_name(&fixture()),
                FuzzInput::Text { .. } => "text",
            })
            .collect();
        kinds.push("level_ready");
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), 17, "{kinds:?}");
        assert!(!kinds.contains(&"unknown"), "{kinds:?}");
    }

    /// The endpoint fixture holds the whole handshake: a fresh server admits the replayed client.
    #[test]
    fn the_endpoint_fixture_reaches_a_connected_peer() {
        let datagrams = unframe(&endpoint_fixture()).expect("framed");
        assert!(datagrams.len() >= 5, "{}", datagrams.len());
        assert!(endpoint_script().is_ok());
    }

    /// The frame fixtures are real traffic: a fresh client applies the server's frames and spawns both crates.
    #[test]
    fn the_frame_fixtures_carry_spawns_and_messages() {
        let to_client = unframe(&frame_to_client_fixture()).expect("framed");
        assert!(!to_client.is_empty());
        assert!(frame_to_client_load(&frame_to_client_fixture()).is_ok());
        assert!(!unframe(&frame_to_server_fixture()).expect("framed").is_empty());
        assert!(frame_to_server_load(&frame_to_server_fixture()).is_ok());
    }

    #[test]
    fn a_relay_reply_round_trips_through_the_local_writer() {
        for t in relay_reply_targets() {
            if let FuzzInput::Bytes { fixture, load } = t.input {
                assert_eq!(load(&fixture()).as_deref(), Ok(&fixture()[..]), "{}", t.name);
            }
        }
    }
}
