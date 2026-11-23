// SPDX-License-Identifier: MPL-2.0
//! Wire 6, part 1 (ADR-8858, which amends ADR-8733 (3)): the resumption ticket in the sealed
//! `Accept`, the client's `ResumeInit` flight and its side of `ResumeReply`, and the part 1
//! server, which reads a flight and refuses it by name to the full handshake.
//!
//! The server's admission of a resume is lane v07-wire6-resume-server's. The client's side is
//! proved here against a scripted server built from the public codec (`TicketKeys`,
//! `ResumeResponder`, `ResumeReply`), which is the code that lane's server will run. Addresses are
//! non-loopback, so `CryptoMode::Auto` seals. Time is a step counter; nothing sleeps.

mod common;

use std::net::SocketAddr;

use common::{Duel, Team};
use regia_net::addr_block::AddrBlock;
use regia_net::crypto::{
    CRYPTO_VERSION, CryptoMode, RESUME_INIT_BYTES, RESUME_INIT_UNANSWERED_SENDS, ResumeCredential,
    ResumeFlight, ResumeHello, ResumeInit, ResumeRefusal, ResumeReply, ResumeResponder, ResumeRetry,
    SealedHeader, SealedKind, SecureLink, ServerKey, TicketClaims, TicketKeys, build_hash,
};
use regia_net::handshake::{
    Accept, ConnectRequest, Deny, PacketKind, RESUME_TICKET_BYTES, RESUMPTION_TICKET_BYTES, split_datagram,
};
use regia_net::prelude::*;
use regia_net::version_probe::{MismatchSide, VersionStamp};
use regia_net::wire::fnv1a_64;
use regia_net::{LoopbackSocket, protocol_id};

type Server = NetServer<Duel, LoopbackSocket>;
type Client = NetClient<Duel, LoopbackSocket>;

const STEP_MS: u64 = 10;
const GRACE_MS: u64 = 60_000;
const PSK: [u8; 32] = [0x5A; 32];
const OLD_TICKET_ID: [u8; RESUME_TICKET_BYTES] = [0xA1; RESUME_TICKET_BYTES];
const NONCE: u64 = 0x5151_0006;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7777))
}

fn client_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 2], port))
}

fn server(net: &LoopbackNet, edit: impl FnOnce(&mut ServerConfig)) -> Server {
    let mut config = ServerConfig {
        resume_grace_ms: GRACE_MS,
        ..ServerConfig::default()
    };
    edit(&mut config);
    NetServer::with_secret(net.bind(server_addr()).expect("bind server"), config, [7; 32])
}

fn client(net: &LoopbackNet, port: u16, edit: impl FnOnce(&mut ClientConfig)) -> Client {
    let mut config = ClientConfig {
        name: "ada".into(),
        ..ClientConfig::default()
    };
    edit(&mut config);
    NetClient::connect(
        net.bind(client_addr(port)).expect("bind client"),
        server_addr(),
        config,
        &Team(1),
    )
    .expect("join fits")
}

/// Steps the client and the server until the client is connected or has ended. Returns the clock
/// and the datagrams the client had sent when its accept arrived (before the flush after it).
fn join(s: &mut Server, c: &mut Client, from: u64) -> (u64, u64) {
    let mut now = from;
    for _ in 0..400 {
        now += STEP_MS;
        c.receive(now);
        if c.is_connected() || matches!(c.state(), ClientState::Ended(_)) {
            let sent = c.totals().datagrams_out;
            c.flush(now);
            s.update(now);
            while s.poll_event().is_some() {}
            return (now, sent);
        }
        c.flush(now);
        s.update(now);
        while s.poll_event().is_some() {}
    }
    (now, c.totals().datagrams_out)
}

/// A few steps of both ends, so the server reads what the client flushed.
fn settle(s: &mut Server, c: &mut Client, from: u64) -> u64 {
    let mut now = from;
    for _ in 0..5 {
        now += STEP_MS;
        c.update(now);
        s.update(now);
        while s.poll_event().is_some() {}
    }
    now
}

fn drain(socket: &mut LoopbackSocket) -> Vec<(Vec<u8>, SocketAddr)> {
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some((n, from)) = socket.recv_from(&mut buf).expect("receive") {
        out.push((buf[..n].to_vec(), from));
    }
    out
}

fn kinds(datagrams: &[(Vec<u8>, SocketAddr)]) -> Vec<PacketKind> {
    datagrams
        .iter()
        .filter_map(|(d, _)| split_datagram(d).map(|(kind, _)| kind))
        .collect()
}

/// The first accept of a sealed link carries a ticket only its server reads, and what it says is
/// the link's: the peer, epoch 1, the client's address block, one grace of life, the protocol,
/// and the 16-byte resume ticket of the same accept.
#[test]
fn a_sealed_accept_carries_a_resumption_ticket_only_its_server_opens() {
    let net = LoopbackNet::new();
    let mut s = server(&net, |_| {});
    let mut c = client(&net, 5000, |_| {});
    let (now, _) = join(&mut s, &mut c, 0);
    assert!(c.is_connected(), "{:?}", c.state());
    assert!(c.is_sealed());
    let peer = c.peer_id().expect("peer");
    let credential = c
        .resumption()
        .expect("a sealed accept carries a resumption ticket");
    assert_eq!(credential.server_key(), c.server_key());
    assert_eq!(credential.flights(), 0);
    let claims = s
        .ticket_keys()
        .open(credential.ticket(), now)
        .expect("the server opens its own ticket");
    assert_eq!(claims.peer, peer.0);
    assert_eq!(claims.epoch, 1);
    assert!(claims.in_block(client_addr(5000).ip()));
    assert!(!claims.in_block(server_addr().ip()));
    assert_eq!(claims.expires_ms - claims.issued_ms, GRACE_MS);
    assert_eq!(claims.check_protocol(protocol_id::<Duel>(), ""), Ok(()));
    assert!(!claims.password_verified, "an open server verified no password");
    assert_eq!(Some(claims.resume_id), c.resume_ticket());
    // Another server (another secret) cannot read it, and it dies with its grace.
    let other = TicketKeys::new(&[8; 32], GRACE_MS);
    assert_eq!(
        other.open(credential.ticket(), now).map(|_| ()),
        Err(ResumeRefusal::TicketAuth)
    );
    assert_eq!(
        s.ticket_keys()
            .open(credential.ticket(), claims.expires_ms)
            .map(|_| ()),
        Err(ResumeRefusal::Expired)
    );
    // The client's first authenticated packet confirmed the accept's epoch (ADR-8858 A2).
    let now = settle(&mut s, &mut c, now);
    let epochs = s.resume_epochs(peer).expect("a live peer has epochs");
    assert_eq!((epochs.confirmed(), epochs.minted()), (1, 1), "at {now} ms");
    assert_eq!(epochs.check(claims.epoch), Ok(()));
}

/// A plain link has no PSK, and a server that keeps no dormant peers mints no ticket: neither
/// accept carries one, and the client has no credential.
#[test]
fn a_plain_link_or_a_server_without_a_grace_gets_no_resumption_ticket() {
    let net = LoopbackNet::new();
    let mut s = server(&net, |config| config.crypto = CryptoMode::Off);
    let mut c = client(&net, 5000, |config| config.crypto = CryptoMode::Off);
    join(&mut s, &mut c, 0);
    assert!(c.is_connected() && !c.is_sealed());
    assert!(
        c.resume_ticket().is_some(),
        "the 16-byte ticket is the plain link's too"
    );
    assert!(c.resumption().is_none(), "no PSK, no resumption ticket");
    drop((s, c));

    let net = LoopbackNet::new();
    let mut s = server(&net, |config| config.resume_grace_ms = 0);
    let mut c = client(&net, 5000, |_| {});
    join(&mut s, &mut c, 0);
    assert!(c.is_connected() && c.is_sealed());
    assert!(c.resume_ticket().is_none());
    assert!(c.resumption().is_none());
}

/// A server that cannot resume a flight refuses it by name with a `Challenge` for the same nonce
/// (ADR-8858 A6): here the resumption ticket outlived its grace while the player's connection
/// stayed up (`resume_expired`). The client carries on from the challenge and comes back as the
/// same peer through the 16-byte ticket in its join token, in the same three datagrams a fresh
/// join takes, with no `ConnectRequest`. The peer's epoch moves on. (Part 1's server refused
/// every flight this way, with `resume_unavailable`; since ADR-8902 it admits the ones it can,
/// `tests/resume_server.rs`.)
#[test]
fn a_refused_resume_is_named_and_the_client_joins_as_the_same_peer_in_three_datagrams() {
    let net = LoopbackNet::new();
    let mut s = server(&net, |_| {});
    let mut first = client(&net, 5000, |_| {});
    let (now, fresh_datagrams) = join(&mut s, &mut first, 0);
    assert!(first.is_connected());
    let mut now = settle(&mut s, &mut first, now);
    let peer = first.peer_id().expect("peer");
    let ticket = first.resume_ticket().expect("ticket");
    let credential = first.resumption().expect("credential");
    assert_eq!(s.last_resume_refusal(), None);
    // The connection stays up past the ticket's grace.
    while now <= GRACE_MS + 1_000 {
        now += 50;
        first.update(now);
        s.update(now);
        while s.poll_event().is_some() {}
    }
    assert!(first.is_connected(), "{:?}", first.state());
    assert_eq!(
        s.ticket_keys().open(credential.ticket(), now).map(|_| ()),
        Err(ResumeRefusal::Expired)
    );

    // The same player from another port (a takeover of the live connection).
    let mut back = client(&net, 5001, |config| {
        config.resume = Some(ticket);
        config.resumption = Some(credential.clone());
    });
    let (now, resume_datagrams) = join(&mut s, &mut back, now);
    assert!(back.is_connected(), "{:?}", back.state());
    assert_eq!(back.peer_id(), Some(peer), "the same peer");
    let refusal = s.last_resume_refusal().expect("the flight was read and refused");
    assert_eq!(refusal, ResumeRefusal::Expired);
    assert_eq!(refusal.as_str(), "resume_expired");
    let totals = s.totals();
    assert_eq!((totals.resume_inits, totals.resume_fallbacks), (1, 1));
    assert_eq!((totals.resumes, totals.resume_takeovers), (1, 1));
    assert_eq!(totals.resumed_one_rtt, 0, "through the full handshake");
    let totals = back.totals();
    assert_eq!((totals.resume_inits, totals.resume_fallbacks), (1, 1));
    assert_eq!(credential.flights(), 1);
    assert_eq!(
        (fresh_datagrams, resume_datagrams),
        (3, 3),
        "ResumeInit stood in for the ConnectRequest: no extra round trip"
    );

    // Epochs: the second accept minted 2; the client's first packet confirmed it.
    let now = settle(&mut s, &mut back, now);
    let epochs = s.resume_epochs(peer).expect("epochs");
    assert_eq!((epochs.confirmed(), epochs.minted()), (2, 2));
    let spent = epochs
        .check(1)
        .expect_err("the first accept's epoch is single use");
    assert_eq!(spent.as_str(), "resume_epoch");
    let next = back.resumption().expect("the second accept carries a new ticket");
    assert_ne!(next.ticket()[..], credential.ticket()[..]);
    let new = s.ticket_keys().open(next.ticket(), now).expect("opens");
    assert_eq!((new.peer, new.epoch), (peer.0, 2));
    assert_eq!(epochs.check(new.epoch), Ok(()));
    assert_eq!(Some(new.resume_id), back.resume_ticket());
}

/// The epochs live on the peer, not on its connection: a player that timed out into dormancy and
/// comes back with its ticket continues the count (ADR-8858 A2), and the ticket of its first
/// accept, still inside its grace, is spent by the confirmation of the second.
#[test]
fn a_dormant_peer_keeps_its_epochs_through_a_resume() {
    let net = LoopbackNet::new();
    let mut s = server(&net, |_| {});
    let mut first = client(&net, 5000, |_| {});
    let (now, _) = join(&mut s, &mut first, 0);
    let mut now = settle(&mut s, &mut first, now);
    let peer = first.peer_id().expect("peer");
    let ticket = first.resume_ticket().expect("ticket");
    let credential = first.resumption().expect("credential");
    // The client goes silent; the server times it out into dormancy.
    let mut dormant = false;
    for _ in 0..600 {
        now += 100;
        s.update(now);
        while s.poll_event().is_some() {}
        if s.dormant_peers().any(|(held, _)| held == peer) {
            dormant = true;
            break;
        }
    }
    assert!(dormant, "the silent player is held for a resume");
    let held = s.resume_epochs(peer).expect("a dormant peer keeps its epochs");
    assert_eq!((held.confirmed(), held.minted()), (1, 1));

    let mut back = client(&net, 5002, |config| {
        config.resume = Some(ticket);
        config.resumption = Some(credential.clone());
    });
    let (now, _) = join(&mut s, &mut back, now);
    assert_eq!(back.peer_id(), Some(peer), "{:?}", back.state());
    let totals = s.totals();
    assert_eq!((totals.resumes, totals.resume_takeovers), (1, 0), "from dormancy");
    let now = settle(&mut s, &mut back, now);
    let epochs = s.resume_epochs(peer).expect("epochs");
    assert_eq!((epochs.confirmed(), epochs.minted()), (2, 2));
    let old = s
        .ticket_keys()
        .open(credential.ticket(), now)
        .expect("inside its grace the first ticket still opens");
    assert_eq!(epochs.check(old.epoch), Err(ResumeRefusal::Epoch));
    let next = back.resumption().expect("credential");
    let new = s.ticket_keys().open(next.ticket(), now).expect("opens");
    assert_eq!((new.peer, new.epoch), (peer.0, 2));
}

/// What a scripted server needs to play the far end of a resume.
struct Script {
    socket: LoopbackSocket,
    keys: TicketKeys,
    client: Client,
    credential: ResumeCredential,
}

fn script(edit: impl FnOnce(&mut ClientConfig)) -> Script {
    let net = LoopbackNet::new();
    let socket = net.bind(server_addr()).expect("bind");
    let mut keys = TicketKeys::new(&[9; 32], GRACE_MS);
    let claims = TicketClaims {
        peer: 5,
        epoch: 1,
        psk: PSK,
        block: AddrBlock::of(client_addr(5000).ip()).key_bytes(),
        issued_ms: 0,
        expires_ms: GRACE_MS,
        protocol_id: protocol_id::<Duel>(),
        build_hash: build_hash(""),
        password_verified: false,
        resume_id: OLD_TICKET_ID,
    };
    let ticket = keys.seal(&claims, 0).expect("seal");
    let credential = ResumeCredential::new(ticket, PSK, None);
    let held = credential.clone();
    let client = client(&net, 5000, |config| {
        config.resume = Some(OLD_TICKET_ID);
        config.resumption = Some(held);
        edit(config);
    })
    .with_request_nonce(NONCE);
    Script {
        socket,
        keys,
        client,
        credential,
    }
}

/// The client's one `ResumeInit`, decoded.
fn only_resume_init(datagrams: &[(Vec<u8>, SocketAddr)]) -> ResumeInit {
    assert_eq!(
        kinds(datagrams),
        vec![PacketKind::ResumeInit],
        "one flight, alone"
    );
    let (bytes, from) = &datagrams[0];
    assert_eq!(*from, client_addr(5000));
    assert_eq!(bytes.len(), RESUME_INIT_BYTES, "padded to 1,200 bytes");
    let (_, body) = split_datagram(bytes).expect("framing");
    ResumeInit::decode(body).expect("decode")
}

/// The server's half, from the public codec: opens the ticket and message 1, writes message 2,
/// and seals an accept with the next ticket under the new link.
fn answer(sc: &mut Script, init: &ResumeInit, now: u64) -> (SecureLink, ResumeReply, Accept) {
    let claims = sc.keys.open(&init.ticket, now).expect("the ticket opens");
    assert!(claims.in_block(client_addr(5000).ip()), "no cookie needed");
    let responder = ResumeResponder::open(
        &claims.psk,
        init.protocol_id,
        init.client_nonce,
        init.crypto_version,
        &init.ticket,
        &init.noise,
    )
    .expect("message 1 opens under the ticket's PSK");
    assert_eq!(responder.hello().name, "ada");
    assert_eq!(responder.hello().join_data, vec![1], "Team(1)");
    let (mut link, noise) = responder.reply(0xBEEF_0006).expect("message 2");
    let next = TicketClaims {
        epoch: claims.epoch + 1,
        psk: *link.resume_psk(),
        issued_ms: now,
        expires_ms: now + GRACE_MS,
        resume_id: [0xA2; RESUME_TICKET_BYTES],
        ..claims
    };
    let accept = Accept {
        client_nonce: init.client_nonce,
        session_token: 0xBEEF_0006,
        peer_id: u16::try_from(claims.peer).expect("a small peer id"),
        tick_rate_hz: 60,
        server_tick: 77,
        resume_ticket: [0xA2; RESUME_TICKET_BYTES],
        resumption: Some(sc.keys.seal(&next, now).expect("seal")),
    };
    let record = link.seal(SealedKind::Accept, &accept.to_body()).expect("seal");
    let reply = ResumeReply {
        client_nonce: init.client_nonce,
        noise,
        accept: record,
    };
    (link, reply, accept)
}

/// The client's side of the whole exchange: one padded datagram out, one reply in, connected.
/// Its next flush is a sealed record under the new keys (the key confirmation the server waits
/// for), and its new credential holds the next ticket and the new link's PSK.
#[test]
fn a_resume_reply_connects_the_client_in_one_round_trip() {
    let mut sc = script(|_| {});
    sc.client.update(STEP_MS);
    let init = only_resume_init(&drain(&mut sc.socket));
    assert_eq!(init.protocol_id, protocol_id::<Duel>());
    assert_eq!(init.client_nonce, NONCE);
    assert_eq!(init.crypto_version, CRYPTO_VERSION);
    assert_eq!(init.cookie, None);
    assert_eq!(init.ticket, *sc.credential.ticket());
    let (mut link, reply, accept) = answer(&mut sc, &init, STEP_MS);
    let reply_bytes = reply.to_datagram().expect("datagram");
    assert!(reply_bytes.len() < RESUME_INIT_BYTES, "no amplification");
    sc.socket.send_to(&reply_bytes, client_addr(5000)).expect("send");

    sc.client.receive(2 * STEP_MS);
    assert!(sc.client.is_connected(), "{:?}", sc.client.state());
    assert_eq!(
        sc.client.totals().datagrams_out,
        1,
        "connected after one datagram: one round trip"
    );
    assert_eq!(sc.client.peer_id(), Some(PeerId(5)));
    assert!(sc.client.is_sealed());
    assert_eq!(sc.client.handshake_hash(), Some(*link.handshake_hash()));
    assert_eq!(sc.client.resume_ticket(), Some(accept.resume_ticket));
    let totals = sc.client.totals();
    assert_eq!(
        (totals.resume_inits, totals.resume_fallbacks, totals.handshakes),
        (1, 0, 1)
    );
    let mut connected = false;
    while let Some(event) = sc.client.poll_event() {
        connected |= matches!(event, ClientEvent::Connected { peer, .. } if peer == PeerId(5));
    }
    assert!(connected);

    // The key confirmation: the client's next flush is a sealed record the new link opens.
    sc.client.flush(2 * STEP_MS);
    let sent = drain(&mut sc.socket);
    assert!(
        !sent.is_empty() && kinds(&sent).iter().all(|kind| *kind == PacketKind::Sealed),
        "{:?}",
        kinds(&sent)
    );
    let (_, body) = split_datagram(&sent[0].0).expect("framing");
    let (kind, _) = link
        .open(SealedHeader::parse(body).expect("header"))
        .expect("only the holder of the flight's ephemeral key seals this");
    assert_eq!(kind, SealedKind::Payload);

    // The next credential: the accept's ticket, and the PSK of the resumed link (never sent).
    let next = sc.client.resumption().expect("credential");
    assert_eq!(Some(*next.ticket()), accept.resumption);
    let hello = ResumeHello {
        flight: 0,
        name: "ada".into(),
        role: PeerRole::Player,
        build: String::new(),
        join_data: vec![1],
    };
    let flight = ResumeFlight::start(&next, protocol_id::<Duel>(), 9, CRYPTO_VERSION, hello).expect("start");
    assert!(
        ResumeResponder::open(
            link.resume_psk(),
            protocol_id::<Duel>(),
            9,
            CRYPTO_VERSION,
            next.ticket(),
            flight.message()
        )
        .is_ok(),
        "the client derived the same next PSK as the server"
    );
    assert!(
        ResumeResponder::open(
            &PSK,
            protocol_id::<Duel>(),
            9,
            CRYPTO_VERSION,
            next.ticket(),
            flight.message()
        )
        .is_err(),
        "and it is not the old one"
    );
    // A second copy of the reply changes nothing.
    sc.socket.send_to(&reply_bytes, client_addr(5000)).expect("send");
    sc.client.update(3 * STEP_MS);
    assert_eq!(sc.client.peer_id(), Some(PeerId(5)));
}

/// A reply whose Noise message was changed is not the server's: ignored, and the real one still
/// works. A reply whose accept was changed gives a link without an accept: the client starts the
/// full handshake over, and a late copy of the real reply no longer counts.
#[test]
fn a_forged_reply_is_ignored_and_a_changed_accept_falls_back_to_the_full_handshake() {
    let mut sc = script(|_| {});
    sc.client.update(STEP_MS);
    let init = only_resume_init(&drain(&mut sc.socket));
    let (_, reply, _) = answer(&mut sc, &init, STEP_MS);

    let mut forged = reply.clone();
    forged.noise[40] ^= 1;
    sc.socket
        .send_to(&forged.to_datagram().expect("datagram"), client_addr(5000))
        .expect("send");
    sc.client.receive(2 * STEP_MS);
    assert_eq!(sc.client.state(), &ClientState::Connecting);
    assert_eq!(sc.client.totals().auth_failed, 1);
    // Another nonce is another attempt's reply.
    let mut other = reply.clone();
    other.client_nonce ^= 1;
    sc.socket
        .send_to(&other.to_datagram().expect("datagram"), client_addr(5000))
        .expect("send");
    sc.client.receive(2 * STEP_MS);
    assert_eq!(sc.client.state(), &ClientState::Connecting);

    let mut changed = reply.clone();
    let last = changed.accept.len() - 1;
    changed.accept[last] ^= 1;
    sc.socket
        .send_to(&changed.to_datagram().expect("datagram"), client_addr(5000))
        .expect("send");
    sc.client.update(3 * STEP_MS);
    assert_eq!(sc.client.state(), &ClientState::Connecting);
    assert_eq!(sc.client.totals().resume_fallbacks, 1);
    let sent = drain(&mut sc.socket);
    assert_eq!(
        kinds(&sent),
        vec![PacketKind::ConnectRequest],
        "a fresh attempt, without the resume"
    );
    let (_, body) = split_datagram(&sent[0].0).expect("framing");
    let request = ConnectRequest::decode(body).expect("request");
    assert_ne!(request.client_nonce, NONCE, "a fresh nonce");
    // The real reply, late: its flight is gone.
    sc.socket
        .send_to(&reply.to_datagram().expect("datagram"), client_addr(5000))
        .expect("send");
    sc.client.update(4 * STEP_MS);
    assert_eq!(sc.client.state(), &ClientState::Connecting);
}

/// A server that cannot tell the client's address from the ticket's block answers `ResumeRetry`.
/// The client sends the same flight again at once, with the cookie: the same Noise message, so
/// the same flight number and the same ephemeral key.
#[test]
fn a_resume_retry_brings_the_same_flight_back_with_the_cookie() {
    let mut sc = script(|_| {});
    sc.client.update(STEP_MS);
    let first = only_resume_init(&drain(&mut sc.socket));
    let retry = ResumeRetry {
        client_nonce: NONCE,
        cookie: [7; 16],
    };
    sc.socket
        .send_to(&retry.to_datagram(), client_addr(5000))
        .expect("send");
    sc.client.update(2 * STEP_MS);
    let second = only_resume_init(&drain(&mut sc.socket));
    assert_eq!(second.cookie, Some([7; 16]));
    assert_eq!(second.noise, first.noise, "the same flight");
    assert_eq!(second.ticket, first.ticket);
    assert_eq!(sc.credential.flights(), 1, "one flight, sent twice");
    // A retry for another nonce is not this attempt's.
    let stray = ResumeRetry {
        client_nonce: NONCE ^ 1,
        cookie: [8; 16],
    };
    sc.socket
        .send_to(&stray.to_datagram(), client_addr(5000))
        .expect("send");
    sc.client.update(3 * STEP_MS);
    assert!(drain(&mut sc.socket).is_empty(), "no resend before the cadence");
    // The reply to the cookie's flight still connects.
    let (_, reply, _) = answer(&mut sc, &second, 3 * STEP_MS);
    sc.socket
        .send_to(&reply.to_datagram().expect("datagram"), client_addr(5000))
        .expect("send");
    sc.client.receive(4 * STEP_MS);
    assert!(sc.client.is_connected());
    assert_eq!(sc.client.totals().resume_fallbacks, 0);
}

/// ADR-8858 A8: a server built before wire 6 drops kind 11 unread. After three flights without
/// any answer the client sends its `ConnectRequest` beside the flight, the old server answers
/// the documented `protocol_mismatch` deny with its stamp, and the client says which side is
/// older. Never a silent mismatch.
#[test]
fn a_server_that_drops_the_resume_kind_gets_a_connect_request_and_its_deny_names_the_older_side() {
    let mut sc = script(|_| {});
    let mut sends: Vec<Vec<PacketKind>> = Vec::new();
    let mut now = 0;
    while sends.len() < 4 {
        now += STEP_MS;
        sc.client.update(now);
        let batch = kinds(&drain(&mut sc.socket));
        if !batch.is_empty() {
            sends.push(batch);
        }
        assert!(now < 2_000, "four sends at the 250 ms cadence");
    }
    assert_eq!(RESUME_INIT_UNANSWERED_SENDS, 3);
    assert_eq!(sends[..3], vec![vec![PacketKind::ResumeInit]; 3]);
    assert_eq!(sends[3], vec![PacketKind::ResumeInit, PacketKind::ConnectRequest]);
    assert_eq!(sc.credential.flights(), 1, "one flight, resent");
    // The old build's answer to the request: the stamped deny of spec 120 § 12.2, wire 5.
    let deny = Deny {
        client_nonce: NONCE,
        reason: DenyReason::ProtocolMismatch,
        detail: "version mismatch".into(),
    };
    let old = VersionStamp::new(5, "", Duel::PROTOCOL_VERSION, 0);
    sc.socket
        .send_to(&deny.to_datagram_with_stamp(&old), client_addr(5000))
        .expect("send");
    sc.client.update(now + STEP_MS);
    assert!(
        matches!(
            sc.client.state(),
            ClientState::Ended(ClientEnd::Denied {
                reason: DenyReason::ProtocolMismatch,
                ..
            })
        ),
        "{:?}",
        sc.client.state()
    );
    let mut side = None;
    while let Some(event) = sc.client.poll_event() {
        if let ClientEvent::VersionMismatch {
            side: s,
            server_is_older,
            ..
        } = event
        {
            side = Some((s, server_is_older));
        }
    }
    assert_eq!(side, Some((MismatchSide::ServerOlder, Some(true))));
}

/// The other direction, on the real server: a `ResumeInit` from a wire 5 build of the same game
/// (it cannot exist, but its protocol id can be claimed) is answered with the stamped
/// `protocol_mismatch` deny naming wire 6, not with a challenge, and nobody joins.
#[test]
fn a_resume_init_of_another_wire_gets_the_stamped_protocol_mismatch_deny() {
    assert_eq!(regia_net::REGIA_NET_WIRE_VERSION, 6);
    let net = LoopbackNet::new();
    let mut s = server(&net, |_| {});
    // The protocol id a wire 5 build of the same game computes (`protocol_id`).
    let mut w = WireWriter::new();
    w.u32(5);
    w.raw(Duel::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(Duel::PROTOCOL_VERSION);
    let old_id = fnv1a_64(w.as_slice());
    assert_ne!(old_id, protocol_id::<Duel>());
    let init = ResumeInit {
        protocol_id: old_id,
        client_nonce: 0xB0B,
        crypto_version: CRYPTO_VERSION,
        cookie: None,
        ticket: [0; RESUMPTION_TICKET_BYTES],
        noise: vec![0; 64],
    };
    let mut old = net.bind(client_addr(4242)).expect("bind");
    let stamp = VersionStamp::new(5, "", Duel::PROTOCOL_VERSION, 0);
    old.send_to(
        &init.to_datagram_with_stamp(&stamp).expect("datagram"),
        server_addr(),
    )
    .expect("send");
    s.update(STEP_MS);
    let answers = drain(&mut old);
    assert_eq!(kinds(&answers), vec![PacketKind::Deny]);
    assert!(answers[0].0.len() < RESUME_INIT_BYTES);
    let (_, body) = split_datagram(&answers[0].0).expect("framing");
    let (deny, server_stamp) = Deny::decode_with_stamp(body).expect("deny");
    assert_eq!(deny.client_nonce, 0xB0B);
    assert_eq!(deny.reason.as_str(), "protocol_mismatch");
    assert_eq!(server_stamp.expect("the server's stamp").wire, 6);
    assert_eq!(s.session().len(), 0, "nobody joined");
    assert_eq!(s.totals().resume_inits, 1);
    // An unpadded flight is not read at all.
    let mut short = init.to_datagram().expect("datagram");
    short.truncate(RESUME_INIT_BYTES - 1);
    old.send_to(&short, server_addr()).expect("send");
    s.update(2 * STEP_MS);
    assert!(drain(&mut old).is_empty(), "no answer to an unpadded flight");
    assert_eq!(s.totals().resume_inits, 1);
}

/// No flight leaves when the link cannot be the credential's: a client that cannot encrypt, or a
/// pinned server key that is not the key the credential's link proved. The connect opens with
/// the `ConnectRequest`, as without a credential.
#[test]
fn a_client_that_cannot_use_its_credential_opens_with_the_connect_request() {
    let mut sc = script(|config| config.crypto = CryptoMode::Off);
    sc.client.update(STEP_MS);
    assert_eq!(kinds(&drain(&mut sc.socket)), vec![PacketKind::ConnectRequest]);
    assert_eq!(sc.client.totals().resume_inits, 0);
    assert_eq!(sc.credential.flights(), 0);

    let mut sc = script(|config| config.server_key = Some(ServerKey([3; 32])));
    sc.client.update(STEP_MS);
    assert_eq!(kinds(&drain(&mut sc.socket)), vec![PacketKind::ConnectRequest]);
    assert_eq!(sc.credential.flights(), 0);
}
