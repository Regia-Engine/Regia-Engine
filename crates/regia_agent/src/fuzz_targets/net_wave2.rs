// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the network paths the first fuzz expansion left (ADR-3283): the plaintext of a
//! sealed record once it authenticated, the client's datagram handler, the bridge's frame parts and
//! the lobby list query. Each is deterministic: fixed fixture keys, a fixed request nonce, a fixed
//! server secret.
//!
//! Registered through `net_trust::targets` (this file is its child module), so the registry list
//! in `mod.rs` is unchanged.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::net::{Ipv4Addr, SocketAddr};

use regia_net::bridge::{FrameAssembler, NetFrame};
use regia_net::connection::{Connection, LinkConfig};
use regia_net::crypto::{
    PLAIN_VERSION, SEALED_RESERVED_HEADER_BYTES, SealedHeader, SealedKind, fixture_link_pair,
};
use regia_net::handshake::{
    self, Accept, ChallengeResponse, ConnectRequest, Deny, DenyReason, JoinToken, PacketKind,
};
use regia_net::lobby::{
    Compare, Distance, LobbyId, LobbyInfo, LobbyQuery, LobbyVisibility, MemberId, NearFilter, NumberFilter,
    StringFilter,
};
use regia_net::session::net_session::SessionWire;
use regia_net::wire::{WireReader, WireWriter};
use regia_net::{
    ClientConfig, ClientEvent, CryptoMode, DatagramSocket, LoopbackNet, LoopbackSocket, NetClient, NetCodec,
    NetServer, PeerRole, ServerConfig,
};

use regia_net::version_probe::{
    MAX_PROBE_ANSWER_BYTES, ProbeAnswer, ProbeRequest, ProbeResponder, VersionStamp, is_probe_answer,
    is_probe_request,
};

use super::{BUG_PREFIX, FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// Records one input holds at most (a mutated count must not loop for long).
const MAX_RECORDS: usize = 64;

/// `u8` count, then `u16` length and bytes per record: a cut input or a hostile count is refused.
fn frame(records: &[Vec<u8>]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.u8(u8::try_from(records.len()).unwrap_or(u8::MAX));
    for r in records.iter().take(usize::from(u8::MAX)) {
        let _ = w.bytes_u16(r, usize::from(u16::MAX));
    }
    w.finish()
}

fn unframe(b: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut r = WireReader::new(b);
    let count = usize::from(r.u8().map_err(e)?);
    if count == 0 || count > MAX_RECORDS {
        return Err(format!("{count} records: 1..={MAX_RECORDS}"));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(r.bytes_u16(usize::from(u16::MAX)).map_err(e)?.to_vec());
    }
    r.finish().map_err(e)?;
    Ok(out)
}

// ---- The sealed record body ----------------------------------------------------------------------

/// The fixture links' key seed and session token: public, so every run seals the same bytes.
const SEED: [u8; 32] = [0x77; 32];
const TOKEN: u32 = 0x0BAD_CAFE;

fn sealed_kind(byte: u8) -> Option<SealedKind> {
    Some(match byte {
        1 => SealedKind::Payload,
        2 => SealedKind::Disconnect,
        3 => SealedKind::Join,
        4 => SealedKind::Accept,
        5 => SealedKind::Deny,
        _ => return None,
    })
}

/// One record: the kind byte, then the plaintext.
fn body_record(kind: SealedKind, plain: &[u8]) -> Vec<u8> {
    let mut out = vec![kind as u8];
    out.extend_from_slice(plain);
    out
}

/// What a client seals for a server and a server for a client: a payload packet with a reliable
/// and an unreliable message, a join token, an accept, a deny and a goodbye.
fn sealed_body_fixture() -> Vec<u8> {
    let mut conn = Connection::new(LinkConfig::default());
    let _ = conn.send(
        regia_net::Channel::GAME_RPC,
        // invariant: encodes a checked-in fixture value, not input; the encoder cannot fail on it.
        vec![1u8, 2, 3].to_bytes().expect("bytes"),
        0,
    );
    let _ = conn.send(
        regia_net::Channel::Unreliable,
        // invariant: encodes a checked-in fixture value, not input; the encoder cannot fail on it.
        vec![4u8].to_bytes().expect("bytes"),
        0,
    );
    let reserved = [0u8; SEALED_RESERVED_HEADER_BYTES];
    let mut records: Vec<Vec<u8>> = conn
        .poll(10, &reserved)
        .iter()
        .map(|p| body_record(SealedKind::Payload, &p[SEALED_RESERVED_HEADER_BYTES..]))
        .collect();
    let mut join = WireWriter::new();
    let token = JoinToken {
        name: "fuzzer".into(),
        role: PeerRole::Player,
        build: "b1".into(),
        password_proof: [7; 32],
        // invariant: encodes a checked-in fixture value, not input; the encoder cannot fail on it.
        join_data: ().to_bytes().expect("join"),
        resume: None,
    };
    if token.encode(&mut join).is_ok() {
        records.push(body_record(SealedKind::Join, &join.finish()));
    }
    let accept = Accept {
        client_nonce: 0x5151,
        session_token: TOKEN,
        peer_id: 3,
        tick_rate_hz: 30,
        server_tick: 1234,
        resume_ticket: [0; 16],
        resumption: None,
    };
    records.push(body_record(SealedKind::Accept, &accept.to_body()));
    let deny = Deny {
        client_nonce: 0x5151,
        reason: DenyReason::Full,
        detail: "server is full".into(),
    };
    records.push(body_record(SealedKind::Deny, &deny.to_body()));
    records.push(body_record(SealedKind::Disconnect, &[1]));
    frame(&records)
}

/// Seals each record's plaintext under the fixture keys (client to server for what a client sends,
/// server to client for `Accept` and `Deny`), opens it on the other side, and parses the plaintext
/// the way the receiving endpoint does after the tag verified: a payload packet through the
/// connection layer, a join token, an accept, a deny, a goodbye's reason byte. The record layer
/// must give back exactly the plaintext it sealed (anything else is a bug); the re-encoding is the
/// records again.
fn sealed_body_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let records = unframe(b)?;
    let (mut client, mut server) = fixture_link_pair(&SEED, TOKEN).map_err(e)?;
    let mut to_server = Connection::new(LinkConfig::default());
    let mut to_client = Connection::new(LinkConfig::default());
    for (now, record) in (10_u64..).step_by(10).zip(&records) {
        let (&kind_byte, plain) = record.split_first().ok_or("empty record")?;
        let kind = sealed_kind(kind_byte).ok_or_else(|| format!("sealed kind {kind_byte}"))?;
        let from_server = matches!(kind, SealedKind::Accept | SealedKind::Deny);
        let (sender, receiver) = if from_server {
            (&mut server, &mut client)
        } else {
            (&mut client, &mut server)
        };
        let datagram = sender.seal(kind, plain).map_err(e)?;
        let Some((PacketKind::Sealed, body)) = handshake::split_datagram(&datagram) else {
            return Err(format!("{BUG_PREFIX}a sealed record is not a sealed datagram"));
        };
        let header =
            SealedHeader::parse(body).ok_or_else(|| format!("{BUG_PREFIX}a sealed record has no header"))?;
        let (opened_kind, opened) = receiver
            .open(header)
            .map_err(|err| format!("{BUG_PREFIX}a record the fixture sealed did not open: {err}"))?;
        if opened_kind != kind || opened != plain {
            return Err(format!("{BUG_PREFIX}the record layer changed a sealed record"));
        }
        match kind {
            SealedKind::Payload => {
                let conn = if from_server {
                    &mut to_client
                } else {
                    &mut to_server
                };
                conn.receive(&opened, now).map_err(e)?;
                while conn.next_message().is_some() {}
            }
            SealedKind::Join => {
                let mut r = WireReader::new(&opened);
                JoinToken::decode(&mut r).map_err(e)?;
                r.finish().map_err(e)?;
            }
            SealedKind::Accept => {
                Accept::decode(&opened).map_err(e)?;
            }
            SealedKind::Deny => {
                Deny::decode(&opened).map_err(e)?;
            }
            SealedKind::Disconnect => {
                if opened.len() != 1 {
                    return Err("a goodbye is one reason byte".into());
                }
            }
        }
    }
    Ok(frame(&records))
}

// ---- The client's datagram path ------------------------------------------------------------------

/// The fixed request nonce of the fuzzed client, so a recorded reply matches it.
const CLIENT_NONCE: u64 = 0x5151_0000_C11E;
/// The fixed secret of the server the fixture is recorded from.
const SERVER_SECRET: [u8; 32] = [0x43; 32];

fn server_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 7777))
}

fn client_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 40_002))
}

/// A plain client with the fixed nonce on `net`, connecting to [`server_addr`].
fn fuzz_client(net: &LoopbackNet) -> Result<NetClient<SessionWire, LoopbackSocket>, String> {
    let socket = net.bind(client_addr()).map_err(e)?;
    let config = ClientConfig {
        name: "fuzzer".into(),
        crypto: CryptoMode::Off,
        ..ClientConfig::default()
    };
    Ok(NetClient::connect(socket, server_addr(), config, &())
        .map_err(e)?
        .with_request_nonce(CLIENT_NONCE))
}

/// What a real fixed-secret server sends the fixed-nonce client: its challenge and accept, a
/// payload packet with two messages, and a goodbye.
fn client_datagram_fixture() -> Vec<u8> {
    client_datagram_script().unwrap_or_default()
}

fn client_datagram_script() -> Result<Vec<u8>, String> {
    let net = LoopbackNet::new();
    let config = ServerConfig {
        crypto: CryptoMode::Off,
        ..ServerConfig::default()
    };
    let mut server: NetServer<SessionWire, LoopbackSocket> =
        NetServer::with_secret(net.bind(server_addr()).map_err(e)?, config, SERVER_SECRET);
    let mut client = net.bind(client_addr()).map_err(e)?;
    let request = ConnectRequest {
        protocol_id: regia_net::protocol_id::<SessionWire>(),
        client_nonce: CLIENT_NONCE,
        crypto_version: PLAIN_VERSION,
        crypto_required: false,
    }
    .to_datagram();
    let challenge_dg = exchange(&mut server, &mut client, &request, 10)?;
    let Some((PacketKind::Challenge, body)) = handshake::split_datagram(&challenge_dg) else {
        return Err("no challenge".into());
    };
    let challenge = handshake::Challenge::decode(body).map_err(e)?;
    let response = ChallengeResponse {
        client_nonce: CLIENT_NONCE,
        cookie: challenge.cookie,
        token: JoinToken {
            name: "fuzzer".into(),
            role: PeerRole::Player,
            build: String::new(),
            password_proof: [0; 32],
            // invariant: encodes a checked-in fixture value, not input; the encoder cannot fail on it.
            join_data: ().to_bytes().expect("join"),
            resume: None,
        },
    }
    .to_datagram()
    .map_err(e)?;
    let accept_dg = exchange(&mut server, &mut client, &response, 20)?;
    let Some((PacketKind::Accept, body)) = handshake::split_datagram(&accept_dg) else {
        return Err("no accept".into());
    };
    let accept = Accept::decode(body).map_err(e)?;
    let mut link = Connection::new(LinkConfig::default());
    // The game lane byte, then the message: what `NetServer::send` writes.
    let message = |m: Vec<u8>| {
        let mut w = WireWriter::new();
        w.u8(0);
        m.encode(&mut w);
        w.finish()
    };
    let _ = link.send(regia_net::Channel::GAME_RPC, message(vec![1, 2, 3]), 0);
    let _ = link.send(regia_net::Channel::Unreliable, message(vec![4]), 0);
    let header = handshake::connected_header(PacketKind::Payload, accept.session_token);
    let mut datagrams = vec![challenge_dg, accept_dg];
    datagrams.extend(link.poll(30, &header));
    let mut goodbye = handshake::connected_header(PacketKind::Disconnect, accept.session_token).to_vec();
    goodbye.push(regia_net::DisconnectReason::Kicked.to_u8());
    datagrams.push(goodbye);
    Ok(frame(&datagrams))
}

/// Sends `datagram` from the raw client socket and returns the server's first reply.
fn exchange(
    server: &mut NetServer<SessionWire, LoopbackSocket>,
    client: &mut LoopbackSocket,
    datagram: &[u8],
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    client.send_to(datagram, server_addr()).map_err(e)?;
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

/// Feeds the framed datagrams to the fixed-nonce client as if the server sent them, one update
/// each, and drains its events. Accepted when the client got as far as connected (or was refused
/// by a decoded `Deny`); a client still connecting at the end read nothing it could use.
fn client_datagram_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let datagrams = unframe(b)?;
    let net = LoopbackNet::new();
    let mut client = fuzz_client(&net)?;
    let mut server = net.bind(server_addr()).map_err(e)?;
    // The first update sends the connect request.
    client.update(0);
    let mut reached = false;
    for (now, d) in (10_u64..).step_by(10).zip(&datagrams) {
        server.send_to(d, client_addr()).map_err(e)?;
        client.update(now);
        while let Some(event) = client.poll_event() {
            reached |= matches!(event, ClientEvent::Connected { .. } | ClientEvent::Ended(_));
        }
        reached |= client.is_connected();
    }
    // Drain what the client sent back, so a test can count it if it wants.
    let mut buf = vec![0u8; 2048];
    while server.recv_from(&mut buf).map_err(e)?.is_some() {}
    if !reached {
        return Err("the client never got past connecting".into());
    }
    Ok(Vec::new())
}

// ---- The bridge's frame parts --------------------------------------------------------------------

/// Three frames of a server: one of three reliable parts, a one-part unreliable frame, and a
/// one-part frame on the unordered RPC stream (wire 6).
fn bridge_frame_fixture() -> Vec<u8> {
    let part = |channel: u8, last: bool, bytes: Vec<u8>| {
        let mut out = vec![channel];
        // invariant: encodes a checked-in fixture value, not input; the encoder cannot fail on it.
        out.extend(NetFrame { last, bytes }.to_bytes().expect("frame"));
        out
    };
    frame(&[
        part(1, false, vec![0x11; 300]),
        part(1, false, vec![0x22; 17]),
        part(1, true, vec![0x33; 5]),
        part(0, true, vec![9, 8, 7]),
        part(2, true, vec![9, 1, 0]),
    ])
}

/// Each record is a channel byte (`1` reliable, `0` unreliable, `2` the unordered RPC stream of
/// wire 6) and a frame part's wire form, fed to a client's and a server's assembler (their limits
/// differ). A reliable frame must come out as the concatenation of its parts; an unreliable or
/// unordered one as its single part (a split part on the unordered stream is refused, and must
/// leave nothing held). At least one frame must complete. The re-encoding is the parts in
/// canonical form.
fn bridge_frame_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let records = unframe(b)?;
    let mut from_server = FrameAssembler::from_server();
    let mut from_client = FrameAssembler::from_client();
    let mut expect: Vec<u8> = Vec::new();
    let mut completed = 0usize;
    let mut canonical = Vec::with_capacity(records.len());
    for record in &records {
        let (&channel, part) = record.split_first().ok_or("empty record")?;
        let reliable = match channel {
            0 => false,
            1 => true,
            2 => {
                let decoded = NetFrame::from_bytes(part).map_err(e)?;
                let held = from_server.assembling_bytes();
                let a = from_server.receive_unordered(part);
                if from_server.assembling_bytes() != held {
                    return Err(format!(
                        "{BUG_PREFIX}an unordered part touched the ordered reassembly"
                    ));
                }
                let whole = a.map_err(e)?;
                if whole.as_ref() != Some(&decoded.bytes) {
                    return Err(format!("{BUG_PREFIX}an unordered frame is not its one part"));
                }
                completed += 1;
                let mut out = vec![channel];
                out.extend(decoded.to_bytes().map_err(e)?);
                canonical.push(out);
                continue;
            }
            other => return Err(format!("channel byte {other}")),
        };
        let decoded = NetFrame::from_bytes(part).map_err(e)?;
        let a = from_server.receive(reliable, part).map_err(e)?;
        // The client limit is smaller: a frame over it is refused there and only there.
        let b = from_client.receive(reliable, part);
        if reliable {
            expect.extend_from_slice(&decoded.bytes);
        }
        if let Some(whole) = &a {
            completed += 1;
            let wanted = if reliable { &expect } else { &decoded.bytes };
            if whole != wanted {
                return Err(format!("{BUG_PREFIX}a reassembled frame is not its parts"));
            }
            if let Ok(Some(other)) = &b
                && other != whole
            {
                return Err(format!("{BUG_PREFIX}the two assemblers disagree"));
            }
        }
        if reliable && decoded.last {
            expect.clear();
        }
        let mut out = vec![channel];
        out.extend(decoded.to_bytes().map_err(e)?);
        canonical.push(out);
    }
    if completed == 0 {
        return Err("no frame completed".into());
    }
    Ok(frame(&canonical))
}

// ---- The lobby list query ------------------------------------------------------------------------

/// Lobbies a query is evaluated against.
fn lobbies() -> Vec<LobbyInfo> {
    let lobby = |id: u64, mode: &str, skill: &str, members: u32| LobbyInfo {
        id: LobbyId(id),
        owner: MemberId(id * 10),
        visibility: LobbyVisibility::Public,
        max_members: 8,
        joinable: true,
        members: Vec::new(),
        member_count: members,
        data: BTreeMap::from([
            ("mode".to_owned(), mode.to_owned()),
            ("skill".to_owned(), skill.to_owned()),
        ]),
        server: None,
    };
    vec![
        lobby(1, "duel", "1200", 1),
        lobby(2, "duel", "1500", 7),
        lobby(3, "raid", "900", 2),
        lobby(4, "duel", "not a number", 8),
    ]
}

// ---- The lobby hub's messages (ADR-8559) ---------------------------------------------------------

/// One request and one event of every kind, framed.
fn lobby_hub_msg_fixture() -> Vec<u8> {
    let records: Vec<Vec<u8>> = regia_net::lobby::hub::sample_messages()
        .iter()
        .filter_map(|m| m.to_bytes().ok())
        .collect();
    frame(&records)
}

/// Decodes each record as a hub message with the hub's bounded decoder and re-encodes it. A decoded
/// message that does not re-encode to bytes decoding to itself would be a bug.
fn lobby_hub_msg_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_net::lobby::hub::HubMessage;
    let records = unframe(b)?;
    let mut out = Vec::with_capacity(records.len());
    for record in &records {
        let message = HubMessage::decode_bounded(record).map_err(e)?;
        let bytes = message
            .to_bytes()
            .map_err(|err| format!("{BUG_PREFIX}a decoded hub message does not re-encode: {err}"))?;
        match HubMessage::decode_bounded(&bytes) {
            Ok(again) if again == message => {}
            _ => return Err(format!("{BUG_PREFIX}a hub message does not round-trip")),
        }
        out.push(bytes);
    }
    Ok(frame(&out))
}

/// A query with every kind of filter, in its JSON form.
fn lobby_query_fixture() -> String {
    let query = LobbyQuery {
        strings: vec![StringFilter {
            key: "mode".into(),
            value: "duel".into(),
            op: Compare::Eq,
        }],
        numbers: vec![NumberFilter {
            key: "skill".into(),
            value: 1000,
            op: Compare::Ge,
        }],
        near: vec![NearFilter {
            key: "skill".into(),
            value: 1300,
        }],
        open_slots: Some(1),
        distance: Distance::Far,
        max_results: 10,
    };
    serde_json::to_string_pretty(&query).unwrap_or_default()
}

/// Parses a query, validates it, evaluates it against fixed lobbies (each result must match the
/// query, and there are at most `max_results`), and writes it back as JSON.
fn lobby_query_load(s: &str) -> Result<String, String> {
    let query: LobbyQuery = serde_json::from_str(s).map_err(e)?;
    query.validate().map_err(e)?;
    let all = lobbies();
    let found = query.evaluate(&all);
    if found.len() > query.max_results as usize {
        return Err(format!("{BUG_PREFIX}more results than max_results"));
    }
    if found.iter().any(|l| !query.matches(l)) {
        return Err(format!("{BUG_PREFIX}a result does not match the query"));
    }
    serde_json::to_string_pretty(&query).map_err(e)
}

/// The version probe's datagrams and the version stamp (spec 120 § 12.2, ADR-7985): a request,
/// an answer and a stamp, framed.
fn version_probe_fixture() -> Vec<u8> {
    let stamp = VersionStamp::new(
        regia_net::REGIA_NET_WIRE_VERSION,
        "1.4.2",
        7,
        0x0123_4567_89ab_cdef,
    );
    let mut tagged = WireWriter::new();
    stamp.encode_tagged(&mut tagged);
    frame(&[
        ProbeRequest { nonce: 0x55 }.to_datagram(),
        ProbeAnswer { nonce: 0x55, stamp }.to_datagram(),
        tagged.finish(),
    ])
}

/// Decodes each record by its magic and re-encodes it. A request must be answered no larger than
/// itself and no answer may outgrow [`MAX_PROBE_ANSWER_BYTES`]: either would be a bug (amplification).
fn version_probe_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let records = unframe(b)?;
    let mut out = Vec::with_capacity(records.len());
    for record in &records {
        if is_probe_request(record) {
            let request = ProbeRequest::decode(record).map_err(e)?;
            let mut responder = ProbeResponder::with_rate(
                VersionStamp::new(u32::MAX, &"9".repeat(64), u32::MAX, u64::MAX),
                0,
            );
            let from = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 1), 9));
            let answer = responder
                .answer(record, from, 0)
                .ok_or_else(|| format!("{BUG_PREFIX}a valid request got no answer"))?;
            if answer.len() > record.len() || answer.len() > MAX_PROBE_ANSWER_BYTES {
                return Err(format!(
                    "{BUG_PREFIX}a {} byte answer to a {} byte probe",
                    answer.len(),
                    record.len()
                ));
            }
            out.push(request.to_datagram());
        } else if is_probe_answer(record) {
            let answer = ProbeAnswer::decode(record).map_err(e)?;
            let bytes = answer.to_datagram();
            if bytes.len() > MAX_PROBE_ANSWER_BYTES {
                return Err(format!("{BUG_PREFIX}a re-encoded answer outgrew the probe"));
            }
            out.push(bytes);
        } else {
            let stamp = VersionStamp::from_tagged(record)
                .map_err(e)?
                .ok_or("not a probe datagram or a version stamp")?;
            let mut w = WireWriter::new();
            stamp.encode_tagged(&mut w);
            out.push(w.finish());
        }
    }
    Ok(frame(&out))
}

/// The ADR-3283 targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        // The version probe and the version stamp (spec 120 § 12.2, ADR-7985).
        FuzzTarget {
            name: "net.version_probe",
            format: None,
            format_version: u32::from(regia_net::version_probe::PROBE_VERSION),
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: version_probe_fixture,
                load: version_probe_load,
            },
        },
        // The plaintext of a sealed record after its tag verified (ADR-3283).
        FuzzTarget {
            name: "net.sealed_body",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: sealed_body_fixture,
                load: sealed_body_load,
            },
        },
        // The client's datagram handler, against a recorded server (ADR-3283).
        FuzzTarget {
            name: "net.client_datagram",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: client_datagram_fixture,
                load: client_datagram_load,
            },
        },
        // The bridge's frame parts and their reassembly (ADR-3283).
        FuzzTarget {
            name: "net.bridge_frame",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: bridge_frame_fixture,
                load: bridge_frame_load,
            },
        },
        // A lobby list query in its JSON form (ADR-3283).
        FuzzTarget {
            name: "net.lobby_query",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: lobby_query_fixture,
                load: lobby_query_load,
            },
        },
        // The self-hosted lobby hub's requests and events, bounded decode (ADR-8559).
        FuzzTarget {
            name: "net.lobby_hub_msg",
            format: None,
            format_version: regia_net::lobby::hub::HUB_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: lobby_hub_msg_fixture,
                load: lobby_hub_msg_load,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_loads_and_repeats() {
        for t in targets() {
            match t.input {
                FuzzInput::Bytes { fixture, load } => {
                    let bytes = fixture();
                    assert!(!bytes.is_empty(), "{}: empty fixture", t.name);
                    let out = load(&bytes).unwrap_or_else(|err| panic!("{}: {err}", t.name));
                    if t.round_trip {
                        assert_eq!(out, bytes, "{}: the fixture is canonical", t.name);
                    }
                    assert_eq!(fixture(), bytes, "{}: deterministic", t.name);
                }
                FuzzInput::Text { fixture, load } => {
                    let text = fixture();
                    assert_eq!(load(&text), Ok(text.clone()), "{}", t.name);
                }
            }
        }
    }

    #[test]
    fn the_client_fixture_connects_and_a_wrong_nonce_does_not() {
        let fixture = client_datagram_fixture();
        assert_eq!(client_datagram_load(&fixture), Ok(Vec::new()));
        // A challenge for another attempt is dropped at the nonce check.
        let mut records = unframe(&fixture).expect("framed");
        records.truncate(1);
        records[0][1] ^= 0xFF;
        assert!(client_datagram_load(&frame(&records)).is_err());
    }

    #[test]
    fn an_unknown_sealed_kind_and_a_long_goodbye_are_refused() {
        assert!(sealed_body_load(&frame(&[vec![9, 1, 2]])).is_err());
        assert!(sealed_body_load(&frame(&[vec![2, 1, 2]])).is_err());
        assert!(sealed_body_load(&frame(&[vec![2, 1]])).is_ok());
    }

    #[test]
    fn a_frame_of_many_full_parts_reassembles() {
        let big = NetFrame {
            last: false,
            bytes: vec![0; regia_net::bridge::FRAME_PART_BYTES],
        };
        let mut records = Vec::new();
        for _ in 0..(regia_net::bridge::MAX_CLIENT_FRAME_BYTES / regia_net::bridge::FRAME_PART_BYTES)
            .min(MAX_RECORDS - 1)
        {
            let mut r = vec![1];
            r.extend(big.to_bytes().expect("frame"));
            records.push(r);
        }
        let mut last = vec![1];
        last.extend(
            NetFrame {
                last: true,
                bytes: vec![1],
            }
            .to_bytes()
            .expect("frame"),
        );
        records.push(last);
        assert!(bridge_frame_load(&frame(&records)).is_ok());
    }

    #[test]
    fn a_bad_lobby_query_is_refused() {
        assert!(lobby_query_load("{}").is_err());
        let text = lobby_query_fixture().replace("\"max_results\": 10", "\"max_results\": 0");
        assert!(lobby_query_load(&text).is_err());
    }
}
