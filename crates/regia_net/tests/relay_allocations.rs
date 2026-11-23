// SPDX-License-Identifier: MPL-2.0
//! Relay protocol 2 allocations over real UDP on 127.0.0.1 (spec 120 § 10.6, ADR-7982, DO27 need
//! "net-relay-many-clients"): one host, 32 relayed clients, each on its own allocation port, every
//! link sealed (`CryptoMode::Required`). The 33rd client over a per-host cap of 32 is refused with
//! `Full`, a forger on a third address cannot spend or block an allocation, and a protocol 1 peer
//! gets a protocol 1 answer or `VersionTooOld`, never silence.
//!
//! Time is a step counter (10 ms per step) and every wait is a bounded step loop on a condition;
//! nothing sleeps and no assertion reads the wall clock.

mod common;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use common::{Duel, DuelMsg, Team};
use regia_net::CryptoMode;
use regia_net::prelude::*;
use regia_net::relay::{
    RelayConfig, RelayCore, RelayReply, RelayServer, RelayTag, encode_allocate, encode_lookup,
    encode_register, encode_register_with_cookie, relay_version, versioned,
};

const STEP_MS: u64 = 10;
const CLIENTS: usize = 32;

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// The host's game socket, shared with the test so the host can register from it (the socket the
/// game uses is the one the relay permits). Relay control datagrams that arrive on it are kept
/// aside instead of reaching the game server, the way a host reads its relay's notices.
#[derive(Clone)]
struct HostSocket {
    inner: Rc<RefCell<UdpTransport>>,
    relay: SocketAddr,
    notices: Rc<RefCell<Vec<RelayReply>>>,
}

impl DatagramSocket for HostSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.inner.borrow_mut().send_to(bytes, dest)
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.inner.borrow_mut().recv_from(buf)?;
            match got {
                Some((n, from)) if from == self.relay => {
                    if let Ok(Some(notice)) = RelayReply::decode(&buf[..n]) {
                        self.notices.borrow_mut().push(notice);
                    }
                }
                other => return Ok(other),
            }
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.borrow().local_addr()
    }
}

/// Sends `request` from `socket` to `to`, runs the relay once and returns the first relay reply.
fn ask(
    relay: &mut RelayServer<UdpTransport>,
    socket: &mut UdpTransport,
    to: SocketAddr,
    request: &[u8],
    now: u64,
) -> Option<RelayReply> {
    socket.send_to(request, to).expect("send");
    let mut buf = [0u8; 2048];
    // Loopback delivery is immediate, but give the kernel a bounded number of relay passes.
    for _ in 0..50 {
        relay.update(now);
        if let Some((n, _)) = socket.recv_from(&mut buf).expect("io") {
            return RelayReply::decode(&buf[..n]).expect("valid reply");
        }
    }
    None
}

struct Rig {
    relay: RelayServer<UdpTransport>,
    relay_addr: SocketAddr,
    server: NetServer<Duel, HostSocket>,
    host_addr: SocketAddr,
    notices: Rc<RefCell<Vec<RelayReply>>>,
    bind_key: [u8; 16],
    now: u64,
}

impl Rig {
    fn new(max_clients_per_host: u32, ports: usize) -> Self {
        let relay_socket = UdpTransport::bind(localhost()).expect("bind relay");
        let relay_addr = relay_socket.local_addr().expect("relay address");
        let pool = (0..ports)
            .map(|_| UdpTransport::bind(localhost()).expect("bind allocation port"))
            .collect();
        let config = RelayConfig {
            max_clients_per_host,
            // Every test client is on 127.0.0.1: one source IP for all of them.
            max_allocations_per_ip: 64,
            ..RelayConfig::default()
        };
        let mut relay =
            RelayServer::from_core(relay_socket, pool, RelayCore::with_config(11, [9; 32], config));
        assert_eq!(relay.status().ports, ports);
        let host = HostSocket {
            inner: Rc::new(RefCell::new(UdpTransport::bind(localhost()).expect("bind host"))),
            relay: relay_addr,
            notices: Rc::new(RefCell::new(Vec::new())),
        };
        let host_addr = host.local_addr().expect("host address");
        let notices = host.notices.clone();
        // The host registers from its game socket (the challenge, then the request with its cookie).
        let mut raw = host.clone();
        let mut buf = [0u8; 2048];
        let mut register = |request: &[u8]| -> RelayReply {
            raw.send_to(request, relay_addr).expect("send");
            for _ in 0..50 {
                relay.update(0);
                if let Some((n, _)) = raw.inner.borrow_mut().recv_from(&mut buf).expect("io") {
                    return RelayReply::decode(&buf[..n])
                        .expect("valid")
                        .expect("relay reply");
                }
            }
            panic!("the relay did not answer the host");
        };
        let RelayReply::RegisterChallenge(cookie) = register(&encode_register(3600, b"t").expect("fits"))
        else {
            panic!("expected a register challenge");
        };
        let RelayReply::JoinCode(code) =
            register(&encode_register_with_cookie(3600, b"t", &cookie).expect("fits"))
        else {
            panic!("expected a join code");
        };
        let mut looker = UdpTransport::bind(localhost()).expect("bind looker");
        let Some(RelayReply::HostInfo { addr, bind_key, .. }) = ask(
            &mut relay,
            &mut looker,
            relay_addr,
            &encode_lookup(&code).expect("code"),
            0,
        ) else {
            panic!("expected host info");
        };
        assert_eq!(addr, host_addr);
        let server = NetServer::with_secret(
            host,
            ServerConfig {
                crypto: CryptoMode::Required,
                max_peers: 40,
                // Every relayed client shares the relay's IP (spec 120 § 10.6, report "For the lead").
                max_peers_per_ip: 0,
                handshakes_per_ip_per_s: 0,
                ..ServerConfig::default()
            },
            [3; 32],
        );
        Self {
            relay,
            relay_addr,
            server,
            host_addr,
            notices,
            bind_key,
            now: 0,
        }
    }

    /// One client's allocation: `Allocate`, its cookie challenge, `Allocate` with the cookie.
    /// Time moves on 100 ms first: every test client shares 127.0.0.1, and the relay limits
    /// control requests per source IP.
    fn allocate(&mut self, socket: &mut UdpTransport, client_key: [u8; 16]) -> RelayReply {
        self.now += 100;
        let first = encode_allocate(self.host_addr, &self.bind_key, &client_key, None);
        match ask(&mut self.relay, socket, self.relay_addr, &first, self.now).expect("an answer") {
            RelayReply::Challenge(cookie) => {
                let again = encode_allocate(self.host_addr, &self.bind_key, &client_key, Some(&cookie));
                ask(&mut self.relay, socket, self.relay_addr, &again, self.now).expect("an answer")
            }
            other => other,
        }
    }

    fn step(&mut self, clients: &mut [NetClient<Duel, UdpTransport>]) {
        self.now += STEP_MS;
        for c in clients.iter_mut() {
            c.update(self.now);
        }
        self.relay.update(self.now);
        self.server.update(self.now);
        self.relay.update(self.now);
    }
}

fn sealed_client(socket: UdpTransport, to: SocketAddr, i: usize) -> NetClient<Duel, UdpTransport> {
    NetClient::connect(
        socket,
        to,
        ClientConfig {
            name: format!("p{i}"),
            crypto: CryptoMode::Required,
            ..ClientConfig::default()
        },
        &Team((i % 2) as u8),
    )
    .expect("join fits")
}

/// Acceptance (N112): 32 clients each reach one host through their own allocation and exchange
/// sealed traffic both ways; the 33rd over a cap of 32 gets `Full { cap: 32 }`. On the base the
/// relay had one flow per host address and answered the second client `Busy`.
#[test]
fn thirty_two_relayed_clients_reach_one_host_sealed_and_the_33rd_is_full() {
    let mut rig = Rig::new(CLIENTS as u32, CLIENTS + 8);
    let mut clients = Vec::new();
    let mut ports = BTreeSet::new();
    for i in 0..CLIENTS {
        let mut socket = UdpTransport::bind(localhost()).expect("bind client");
        let mut key = [0u8; 16];
        key[0] = i as u8;
        key[15] = 0xC1;
        let RelayReply::Allocated { port, peer } = rig.allocate(&mut socket, key) else {
            panic!("client {i} got no allocation");
        };
        assert_eq!(peer, rig.host_addr);
        assert!(ports.insert(port), "client {i} got a port another client holds");
        let via = SocketAddr::new(rig.relay_addr.ip(), port);
        clients.push(sealed_client(socket, via, i));
    }
    assert_eq!(rig.relay.status().allocations, CLIENTS);
    assert_eq!(rig.relay.core().clients_of(rig.host_addr), CLIENTS as u32);

    // The 33rd client: refused with the cap, nothing reserved.
    let mut extra = UdpTransport::bind(localhost()).expect("bind 33rd");
    assert_eq!(
        rig.allocate(&mut extra, [0x33; 16]),
        RelayReply::Full { cap: CLIENTS as u16 }
    );
    assert_eq!(rig.relay.status().allocations, CLIENTS);

    for _ in 0..600 {
        if clients.iter().all(|c| c.is_connected()) {
            break;
        }
        rig.step(&mut clients);
    }
    let joined = rig
        .notices
        .borrow()
        .iter()
        .filter(|n| matches!(n, RelayReply::ClientJoined { .. }))
        .count();
    assert_eq!(joined, CLIENTS, "the host heard of every allocation");
    let connected = clients.iter().filter(|c| c.is_connected()).count();
    assert_eq!(connected, CLIENTS, "every relayed client joined");
    assert!(
        clients.iter().all(|c| c.is_sealed()),
        "every relayed link is sealed"
    );
    // The host sees each relayed client as its own address: the relay's IP and that client's port.
    let seen: BTreeSet<SocketAddr> = rig.server.session().peers().map(|p| p.addr).collect();
    let expected: BTreeSet<SocketAddr> = ports
        .iter()
        .map(|p| SocketAddr::new(rig.relay_addr.ip(), *p))
        .collect();
    assert_eq!(seen, expected);

    // Traffic both ways: each client says its name, the host answers each peer by name.
    for (i, c) in clients.iter_mut().enumerate() {
        c.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("hello from p{i}")))
            .expect("queued");
    }
    let mut heard = BTreeSet::new();
    for _ in 0..300 {
        rig.step(&mut clients);
        while let Some(event) = rig.server.poll_event() {
            if let ServerEvent::Message {
                peer,
                message: DuelMsg::Chat(text),
                ..
            } = event
            {
                heard.insert(text.clone());
                rig.server
                    .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(format!("ack {text}")))
                    .expect("queued");
            }
        }
        if heard.len() == CLIENTS {
            break;
        }
    }
    assert_eq!(heard.len(), CLIENTS, "the host heard every client");
    let mut acked = vec![false; CLIENTS];
    for _ in 0..300 {
        rig.step(&mut clients);
        for (i, c) in clients.iter_mut().enumerate() {
            while let Some(event) = c.poll_event() {
                if let ClientEvent::Message {
                    message: DuelMsg::Chat(text),
                    ..
                } = event
                {
                    assert_eq!(
                        text,
                        format!("ack hello from p{i}"),
                        "client {i} got another client's answer"
                    );
                    acked[i] = true;
                }
            }
        }
        if acked.iter().all(|a| *a) {
            break;
        }
    }
    assert!(
        acked.iter().all(|a| *a),
        "every client heard its own answer: {acked:?}"
    );
    assert!(rig.relay.status().forwarded > 0);
}

/// Acceptance (N112): a forger on a third address that knows an allocation's port can neither
/// spend it nor block it, and the client's own re-`Allocate` from its address is answered.
#[test]
fn a_forger_on_a_third_address_cannot_spend_or_block_an_allocation() {
    let mut rig = Rig::new(8, 4);
    let mut socket = UdpTransport::bind(localhost()).expect("bind client");
    let key = [0x5A; 16];
    let RelayReply::Allocated { port, .. } = rig.allocate(&mut socket, key) else {
        panic!("no allocation");
    };
    let via = SocketAddr::new(rig.relay_addr.ip(), port);
    let mut clients = vec![sealed_client(socket, via, 0)];
    // The forger floods the port with more than the allocation's 4 MiB/s budget.
    let mut forger = UdpTransport::bind(localhost()).expect("bind forger");
    let junk = [0xEE; 1200];
    for _ in 0..40 {
        for _ in 0..100 {
            forger.send_to(&junk, via).expect("send");
        }
        rig.relay.update(rig.now);
    }
    for _ in 0..600 {
        if clients[0].is_connected() {
            break;
        }
        rig.step(&mut clients);
    }
    assert!(
        clients[0].is_connected(),
        "the client joined through the flooded allocation"
    );
    let status = rig.relay.status();
    assert!(
        status.permission_dropped > 0,
        "the forger's datagrams were dropped and counted"
    );
    assert_eq!(
        status.forward_dropped, 0,
        "the forger spent none of the allocation's budget"
    );
    assert_eq!(
        rig.server.session().peers().count(),
        1,
        "the forger is not a peer"
    );
    // The forger also cannot take the allocation over with the client's key it does not have,
    // nor push the client off with an `Allocate` of its own.
    let mut forged_key = key;
    forged_key[0] ^= 1;
    let reply = rig.allocate(&mut forger, forged_key);
    let RelayReply::Allocated { port: other, .. } = reply else {
        panic!("a bind-key holder gets its own allocation, got {reply:?}");
    };
    assert_ne!(other, port, "the forger got a port of its own, not the client's");
    for _ in 0..50 {
        rig.step(&mut clients);
    }
    assert!(clients[0].is_connected(), "the client still plays");
}

/// Acceptance (N112): a protocol 1 peer against this protocol 2 relay gets protocol 1 answers for
/// protocol 1 operations and `VersionTooOld { min: 2 }` for an allocation operation; a protocol 3
/// request is answered `VersionTooNew`. Never silence.
#[test]
fn a_protocol_1_client_gets_its_answer_or_version_too_old() {
    let mut rig = Rig::new(8, 4);
    let relay = rig.relay_addr;
    let mut v1 = UdpTransport::bind(localhost()).expect("bind v1 client");
    let mut buf = [0u8; 2048];
    // A protocol 1 lookup of an unknown code: a protocol 1 answer, framed as protocol 1.
    v1.send_to(&encode_lookup("AAAAAA").expect("code"), relay)
        .expect("send");
    let mut answer = None;
    for _ in 0..50 {
        rig.relay.update(0);
        if let Some((n, _)) = v1.recv_from(&mut buf).expect("io") {
            answer = Some(buf[..n].to_vec());
            break;
        }
    }
    let answer = answer.expect("a protocol 1 lookup is answered");
    assert_eq!(relay_version(&answer), Some(1));
    assert!(matches!(
        RelayReply::decode(&answer),
        Ok(Some(RelayReply::Error(_)))
    ));
    // An allocation operation in protocol 1 framing.
    let mut old_allocate = encode_allocate(rig.host_addr, &rig.bind_key, &[1; 16], None);
    old_allocate.drain(4..6);
    assert_eq!(old_allocate[4], RelayTag::Allocate as u8);
    assert_eq!(
        ask(&mut rig.relay, &mut v1, relay, &old_allocate, 0),
        Some(RelayReply::VersionTooOld { min: 2 })
    );
    // The same lookup in protocol 2 framing is answered in protocol 2 framing.
    v1.send_to(&versioned(&encode_lookup("AAAAAA").expect("code")), relay)
        .expect("send");
    let mut v2_answer = None;
    for _ in 0..50 {
        rig.relay.update(0);
        if let Some((n, _)) = v1.recv_from(&mut buf).expect("io") {
            v2_answer = Some(buf[..n].to_vec());
            break;
        }
    }
    assert_eq!(relay_version(&v2_answer.expect("answered")), Some(2));
    // A newer protocol than this relay speaks (protocol 3 is the relay's own since ADR-8340).
    let mut newer = versioned(&encode_lookup("AAAAAA").expect("code"));
    newer[5] = regia_net::relay::RELAY_PROTOCOL_VERSION + 1;
    assert_eq!(
        ask(&mut rig.relay, &mut v1, relay, &newer, 0),
        Some(RelayReply::VersionTooNew {
            max: regia_net::relay::RELAY_PROTOCOL_VERSION
        })
    );
}
