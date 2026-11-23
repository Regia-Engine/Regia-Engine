// SPDX-License-Identifier: MPL-2.0
//! Version refusals name the older side (spec 120 § 12.2, ADR-7985): stamped `protocol_mismatch`
//! denies, the plaintext version probe, builds that predate either, and run-time content hashes.
//! Over the in-memory loopback network; time is a step counter, nothing sleeps.

use std::net::SocketAddr;

use regia_net::LoopbackSocket;
use regia_net::handshake::{ConnectRequest, Deny, split_datagram};
use regia_net::payload::protocol_id;
use regia_net::prelude::*;
use regia_net::version_probe::{
    MAX_PROBE_ANSWER_BYTES, MismatchSide, PROBE_ANSWERS_PER_PREFIX_PER_S, PROBE_REQUEST_BYTES, PROBE_WAIT_MS,
    ProbeAnswer, ProbeRequest, VersionStamp, is_probe_request,
};
use regia_net::wire::{WireError, WireReader, WireWriter};

const STEP_MS: u64 = 10;

#[derive(Debug, Clone, PartialEq)]
struct Bytes(Vec<u8>);

impl NetCodec for Bytes {
    fn encode(&self, w: &mut WireWriter) {
        self.0.encode(w);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(Vec::<u8>::decode(r)?))
    }
}

/// The game as release 1.2.0 shipped it.
#[derive(Debug)]
struct Old;
impl NetGame for Old {
    type Message = Bytes;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "version-probe-tests";
    const PROTOCOL_VERSION: u32 = 4;
    const GAME_VERSION: &'static str = "1.2.0";
}

/// The same game, release 1.3.0, with a new message set.
#[derive(Debug)]
struct New;
impl NetGame for New {
    type Message = Bytes;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "version-probe-tests";
    const PROTOCOL_VERSION: u32 = 5;
    const GAME_VERSION: &'static str = "1.3.0";
}

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, last], port))
}

fn server<G: NetGame>(net: &LoopbackNet) -> NetServer<G, LoopbackSocket> {
    NetServer::with_secret(
        net.bind(addr(1, 7777)).expect("bind"),
        ServerConfig::default(),
        [3; 32],
    )
}

fn client<G: NetGame<JoinData = ()>>(net: &LoopbackNet) -> NetClient<G, LoopbackSocket> {
    NetClient::connect(
        net.bind(addr(1, 0)).expect("bind"),
        addr(1, 7777),
        ClientConfig::default(),
        &(),
    )
    .expect("join fits")
}

/// Runs both until the client raised `VersionMismatch` (or `steps` passed); returns every client
/// event in order.
fn run<S: NetGame, C: NetGame<JoinData = ()>>(
    server: &mut NetServer<S, LoopbackSocket>,
    client: &mut NetClient<C, LoopbackSocket>,
    steps: u64,
) -> Vec<ClientEvent<C>> {
    let mut events = Vec::new();
    for i in 1..=steps {
        let now = i * STEP_MS;
        client.update(now);
        server.update(now);
        events.extend(std::iter::from_fn(|| client.poll_event()));
        if events
            .iter()
            .any(|e| matches!(e, ClientEvent::VersionMismatch { .. }))
        {
            break;
        }
    }
    events
}

fn denied_detail<G: NetGame + std::fmt::Debug>(events: &[ClientEvent<G>]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            ClientEvent::Ended(ClientEnd::Denied {
                reason: DenyReason::ProtocolMismatch,
                detail,
            }) => Some(detail.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no protocol_mismatch deny in {events:?}"))
}

fn mismatch<G: NetGame + std::fmt::Debug>(
    events: &[ClientEvent<G>],
) -> (Option<String>, String, Option<bool>, MismatchSide, String) {
    events
        .iter()
        .find_map(|e| match e {
            ClientEvent::VersionMismatch {
                server_game_version,
                ours,
                server_is_older,
                side,
                message,
            } => Some((
                server_game_version.clone(),
                ours.clone(),
                *server_is_older,
                *side,
                message.clone(),
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no VersionMismatch in {events:?}"))
}

#[test]
fn an_old_client_is_told_to_update_the_game() {
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    let mut c = client::<Old>(&net);
    let events = run(&mut s, &mut c, 50);
    let detail = denied_detail(&events);
    assert!(
        detail.contains("update the game") && detail.ends_with("(client_older)"),
        "{detail}"
    );
    assert!(detail.contains("1.3.0") && detail.contains("1.2.0"), "{detail}");
    let (server_version, ours, server_is_older, side, message) = mismatch(&events);
    assert_eq!(server_version.as_deref(), Some("1.3.0"));
    assert_eq!(ours, "1.2.0");
    assert_eq!(server_is_older, Some(false));
    assert_eq!(side, MismatchSide::ClientOlder);
    assert!(message.contains("update the game"), "{message}");
    // The stamped deny named the server at once: no probe was needed.
    assert!(!c.probe_pending());
    assert_eq!(s.probe_responder().answered(), 0);
    let status = c.status();
    assert_eq!(
        status.version_mismatch.as_ref().map(|r| r.side),
        Some(MismatchSide::ClientOlder)
    );
    assert_eq!(
        s.status().version_mismatch.map(|r| r.side),
        Some(MismatchSide::ClientOlder)
    );
}

#[test]
fn a_new_client_is_told_the_server_is_older() {
    let net = LoopbackNet::new();
    let mut s = server::<Old>(&net);
    let mut c = client::<New>(&net);
    let events = run(&mut s, &mut c, 50);
    let detail = denied_detail(&events);
    assert!(
        detail.contains("the server is older than this build") && detail.ends_with("(server_older)"),
        "{detail}"
    );
    let (server_version, ours, server_is_older, side, _) = mismatch(&events);
    assert_eq!(server_version.as_deref(), Some("1.2.0"));
    assert_eq!(ours, "1.3.0");
    assert_eq!(server_is_older, Some(true));
    assert_eq!(side, MismatchSide::ServerOlder);
}

/// A client built before stamps sends a bare request and reads a deny with nothing after the
/// detail; the text alone must tell its player to update.
#[test]
fn a_client_that_predates_stamps_reads_a_plain_deny_that_names_its_side() {
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    let mut old = net.bind(addr(1, 4000)).expect("bind");
    let request = ConnectRequest {
        protocol_id: protocol_id::<Old>(),
        client_nonce: 99,
        crypto_version: 0,
        crypto_required: false,
    }
    .to_datagram();
    old.send_to(&request, addr(1, 7777)).expect("send");
    s.update(STEP_MS);
    let mut buf = [0u8; 512];
    let (n, _) = old.recv_from(&mut buf).expect("io").expect("a deny");
    let (_, body) = split_datagram(&buf[..n]).expect("regia traffic");
    let deny = Deny::decode(body).expect("an old build's strict decode reads it");
    assert_eq!(deny.reason, DenyReason::ProtocolMismatch);
    assert!(deny.detail.contains("update the game"), "{}", deny.detail);
    assert_eq!(
        MismatchSide::in_detail(&deny.detail),
        Some(MismatchSide::ClientOlder)
    );
}

/// A server built before the probe answers `protocol_mismatch` with a bare deny and drops the
/// probe: after `PROBE_WAIT_MS` the client concludes the server is older.
#[test]
fn no_probe_answer_after_a_bare_deny_means_the_server_is_older() {
    let net = LoopbackNet::new();
    let mut old_server = net.bind(addr(1, 7777)).expect("bind");
    let mut c = client::<New>(&net);
    let mut probes = 0;
    let mut events = Vec::new();
    let mut buf = [0u8; 2048];
    for i in 1..=((PROBE_WAIT_MS / STEP_MS) + 20) {
        let now = i * STEP_MS;
        c.update(now);
        while let Ok(Some((n, from))) = old_server.recv_from(&mut buf) {
            let bytes = &buf[..n];
            if is_probe_request(bytes) {
                probes += 1;
                continue; // An old build does not know the probe.
            }
            if let Some((_, body)) = split_datagram(bytes)
                && let Ok(req) = ConnectRequest::decode(body)
            {
                let deny = Deny {
                    client_nonce: req.client_nonce,
                    reason: DenyReason::ProtocolMismatch,
                    detail: "protocol mismatch".into(),
                };
                old_server.send_to(&deny.to_datagram(), from).expect("send");
            }
        }
        events.extend(std::iter::from_fn(|| c.poll_event()));
        if events
            .iter()
            .any(|e| matches!(e, ClientEvent::VersionMismatch { .. }))
        {
            assert!(
                now >= PROBE_WAIT_MS,
                "resolved at {now} ms, before the wait ran out"
            );
            break;
        }
    }
    assert_eq!(denied_detail(&events), "protocol mismatch");
    let (server_version, ours, server_is_older, side, message) = mismatch(&events);
    assert_eq!(server_version, None);
    assert_eq!(ours, "1.3.0");
    assert_eq!(server_is_older, Some(true));
    assert_eq!(side, MismatchSide::ServerOlder);
    assert!(
        message.contains("the server is older than this build"),
        "{message}"
    );
    assert!(probes >= 2, "the probe is resent while it waits ({probes})");
}

/// A server that sends a bare deny but answers the probe (a tool, or a later build behind a
/// layer that strips the stamp) is named by its answer.
#[test]
fn a_probe_answer_names_the_server() {
    let net = LoopbackNet::new();
    let mut odd_server = net.bind(addr(1, 7777)).expect("bind");
    let mut c = client::<Old>(&net);
    let mut events = Vec::new();
    let mut buf = [0u8; 2048];
    for i in 1..=100 {
        let now = i * STEP_MS;
        c.update(now);
        while let Ok(Some((n, from))) = odd_server.recv_from(&mut buf) {
            let bytes = buf[..n].to_vec();
            if is_probe_request(&bytes) {
                let req = ProbeRequest::decode(&bytes).expect("probe");
                let answer = ProbeAnswer {
                    nonce: req.nonce,
                    stamp: VersionStamp::of::<New>(),
                };
                odd_server.send_to(&answer.to_datagram(), from).expect("send");
            } else if let Some((_, body)) = split_datagram(&bytes)
                && let Ok(req) = ConnectRequest::decode(body)
            {
                let deny = Deny {
                    client_nonce: req.client_nonce,
                    reason: DenyReason::ProtocolMismatch,
                    detail: "protocol mismatch".into(),
                };
                odd_server.send_to(&deny.to_datagram(), from).expect("send");
            }
        }
        events.extend(std::iter::from_fn(|| c.poll_event()));
        if events
            .iter()
            .any(|e| matches!(e, ClientEvent::VersionMismatch { .. }))
        {
            assert!(now < PROBE_WAIT_MS, "the answer resolved it, not the timeout");
            break;
        }
    }
    let (server_version, _, server_is_older, side, _) = mismatch(&events);
    assert_eq!(server_version.as_deref(), Some("1.3.0"));
    assert_eq!(server_is_older, Some(false));
    assert_eq!(side, MismatchSide::ClientOlder);
}

#[test]
fn the_server_answers_the_probe_no_larger_than_the_request() {
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    let mut asker = net.bind(addr(9, 5000)).expect("bind");
    let request = ProbeRequest { nonce: 0xfeed }.to_datagram();
    assert_eq!(request.len(), PROBE_REQUEST_BYTES);
    asker.send_to(&request, addr(1, 7777)).expect("send");
    s.update(STEP_MS);
    let mut buf = [0u8; 512];
    let (n, _) = asker.recv_from(&mut buf).expect("io").expect("an answer");
    assert!(n <= request.len() && n <= MAX_PROBE_ANSWER_BYTES, "{n}");
    let answer = ProbeAnswer::decode(&buf[..n]).expect("answer");
    assert_eq!(answer.nonce, 0xfeed);
    assert_eq!(answer.stamp, VersionStamp::of::<New>());
    // An unpadded probe gets nothing (it could amplify).
    asker.send_to(&request[..20], addr(1, 7777)).expect("send");
    s.update(2 * STEP_MS);
    assert!(asker.recv_from(&mut buf).expect("io").is_none());
}

#[test]
fn the_server_limits_probe_answers_per_prefix() {
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    let burst = PROBE_ANSWERS_PER_PREFIX_PER_S as usize * 2;
    let mut askers: Vec<LoopbackSocket> = (0..burst * 2)
        .map(|i| net.bind(addr(10 + i as u8, 5000)).expect("bind"))
        .collect();
    let request = ProbeRequest { nonce: 1 }.to_datagram();
    for a in &mut askers {
        a.send_to(&request, addr(1, 7777)).expect("send");
    }
    s.update(STEP_MS);
    let mut buf = [0u8; 512];
    let answered = askers
        .iter_mut()
        .map(|a| a.recv_from(&mut buf).expect("io").is_some())
        .filter(|answered| *answered)
        .count();
    assert_eq!(answered, burst, "every address of 127.0.0.0/24 shares one budget");
    assert_eq!(s.probe_responder().shed(), burst as u64);
}

/// Same build, other content (a run-time content hash such as a mounted data set): refused with
/// `content_differs`, and no side is older.
#[test]
fn other_content_is_refused_with_content_differs() {
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    s.set_content_hash(0x1111);
    let mut c = client::<New>(&net);
    c.set_content_hash(0x2222);
    let events = run(&mut s, &mut c, 50);
    let detail = denied_detail(&events);
    assert!(detail.ends_with("(content_differs)"), "{detail}");
    let (_, _, server_is_older, side, message) = mismatch(&events);
    assert_eq!(side, MismatchSide::ContentDiffers);
    assert_eq!(server_is_older, None);
    assert!(message.contains("install the same content"), "{message}");

    // The same content joins.
    let net = LoopbackNet::new();
    let mut s = server::<New>(&net);
    s.set_content_hash(0x1111);
    let mut c = client::<New>(&net);
    c.set_content_hash(0x1111);
    for i in 1..=100 {
        c.update(i * STEP_MS);
        s.update(i * STEP_MS);
        if c.is_connected() {
            break;
        }
    }
    assert!(c.is_connected(), "{:?}", c.state());
}
