// SPDX-License-Identifier: MPL-2.0
//! One UDP port for both layers (ADR-2665 (6), the open option of ADR-2039 (8) and ADR-2440 (1)).
//!
//! With the server binary's `--shared-port` a replication game under the host serves its link
//! (replication frames) **and** the host's session (`NetSession`: join, chat, remote admin,
//! streams) on the game port. [`SharedPort::split`] turns one socket into two
//! [`DatagramSocket`] halves, one per layer, and routes each inbound datagram:
//!
//! - a `ConnectRequest` by its protocol id: the link game's id goes to the link half, any other
//!   id to the session half (whose endpoint denies a wrong id as before). This needs no state, so
//!   a spoofed request costs nothing;
//! - a `ResumeInit` (wire 6, ADR-8858) the same way: it stands in for a `ConnectRequest` and
//!   carries the protocol id in the same place;
//! - any other datagram by its source address's route. A half claims an address when it sends it
//!   a datagram that only follows a checked cookie (`Accept`, `KeyReply`, `Payload`, `Sealed`), so
//!   routes exist only for addresses that completed a handshake;
//! - a handshake reply (`KeyInit`, `ChallengeResponse`) to the half that sent the address its last
//!   `Challenge`, also past an older route, so a client can rejoin from the same address on the
//!   other layer. That table is bounded, and forged requests can push a real client's row out, so
//!   a reply with no row goes to **both** halves: each checks the cookie it minted and drops the
//!   other's without an answer;
//! - a `Sealed` record by its session token, when a half has sent records under that token: this
//!   is what lets a peer whose NAT moved it reach the layer that can authenticate it, also from
//!   an address with an older route. The token only picks the half. The record must still open
//!   under that peer's key before anything changes;
//! - a version probe (`RGVP`, spec 120 § 12.2) to the half the source already talks to, else to
//!   the link half, whose server answers it with the game's versions;
//! - a datagram with none of these goes to the session half (the old single-layer behavior: it
//!   drops what it does not know).
//!
//! Every queue and table is bounded: a flood drops datagrams (counted), it never grows memory. A
//! half reads at most [`MAX_READS_PER_ROUND`] datagrams before it reports the socket empty, so a
//! flood faster than the read loop cannot hold a frame either. A queued datagram is at most
//! [`MAX_DATAGRAM_BYTES`] (what every endpoint reads; a longer one is dropped, not copied), and a
//! half's queue holds at most [`MAX_QUEUED_BYTES`]; drops are counted per source
//! ([`SharedPort::drops_by_source`]; spec 120 § 11.5, adversarial row 14, ADR-7981).

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use crate::crypto::SealedHeader;
use crate::handshake::{ConnectRequest, MAGIC, PacketKind};
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};

/// Most datagrams waiting per half before new ones are dropped.
pub const MAX_QUEUED_DATAGRAMS: usize = 4096;
/// Most claimed routes (addresses that completed a handshake on either layer).
pub const MAX_ROUTES: usize = 8192;
/// Most addresses remembered between a challenge and its reply.
pub const MAX_PENDING_CHALLENGES: usize = 4096;
/// Most session tokens remembered, each with the layer that sends under it.
pub const MAX_TOKENS: usize = 8192;
/// Most datagrams one half reads from the socket between two reports of "nothing waiting": enough
/// to fill both queues, and a bound on what one endpoint's receive can cost under a flood.
pub const MAX_READS_PER_ROUND: usize = 2 * MAX_QUEUED_DATAGRAMS;
/// The read buffer: the largest datagram the operating system can hand over, so a longer one is
/// seen whole (and dropped) rather than cut; one buffer per port, reused.
const READ_BUFFER: usize = 65_536;
/// Most bytes of datagrams waiting per half (spec 120 § 11.5): a full queue of 1 KiB datagrams.
/// Without it a half could hold [`MAX_QUEUED_DATAGRAMS`] x [`MAX_DATAGRAM_BYTES`] (8 MiB).
pub const MAX_QUEUED_BYTES: usize = MAX_QUEUED_DATAGRAMS * 1024;
/// Most sources [`SharedPort::drops_by_source`] remembers; the oldest is forgotten past it.
pub const MAX_DROP_SOURCES: usize = 1024;

/// Which layer a datagram belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortLayer {
    /// The host's session (`NetSession`).
    Session,
    /// The game's link (`NetLink`, replication frames).
    Link,
}

impl PortLayer {
    fn index(self) -> usize {
        match self {
            Self::Session => 0,
            Self::Link => 1,
        }
    }
}

/// Where an inbound datagram goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    One(PortLayer),
    Both,
}

/// Counters of a shared port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedPortStats {
    /// Datagrams handed to each half (session, link).
    pub routed: [u64; 2],
    /// Datagrams dropped because a half's queue was full (by count or by
    /// [`MAX_QUEUED_BYTES`]).
    pub dropped_full: u64,
    /// Datagrams dropped because they were longer than [`MAX_DATAGRAM_BYTES`].
    pub dropped_oversize: u64,
    /// Bytes waiting in each half's queue now (session, link).
    pub queued_bytes: [usize; 2],
    /// Datagrams that were not `regia_net` datagrams (wrong magic or too short).
    pub dropped_foreign: u64,
    /// Addresses with a claimed route now.
    pub routes: usize,
}

struct Inner {
    socket: Box<dyn DatagramSocket + Send>,
    link_protocol: u64,
    queues: [VecDeque<(Vec<u8>, SocketAddr)>; 2],
    routes: HashMap<SocketAddr, PortLayer>,
    /// Insertion order of `routes`, oldest first (for the bound).
    route_order: VecDeque<SocketAddr>,
    pending: HashMap<SocketAddr, PortLayer>,
    pending_order: VecDeque<SocketAddr>,
    /// The layer sending `Sealed` records under each session token.
    tokens: HashMap<u32, PortLayer>,
    token_order: VecDeque<u32>,
    /// Reads each half may still make before it next reports the socket empty.
    reads_left: [usize; 2],
    stats: SharedPortStats,
    /// Datagrams dropped (full or oversize) per source, bounded by [`MAX_DROP_SOURCES`].
    drops: HashMap<SocketAddr, u64>,
    drop_order: VecDeque<SocketAddr>,
    buf: Vec<u8>,
}

fn kind_of(bytes: &[u8]) -> Option<PacketKind> {
    if bytes.len() < 5 || bytes[..4] != MAGIC {
        return None;
    }
    PacketKind::from_u8(bytes[4])
}

fn remember<K: Copy + Eq + Hash, V: Copy>(
    map: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    cap: usize,
    key: K,
    value: V,
) {
    if map.insert(key, value).is_none() {
        order.push_back(key);
        while map.len() > cap {
            match order.pop_front() {
                Some(old) => {
                    map.remove(&old);
                }
                None => break,
            }
        }
    }
}

impl Inner {
    /// Where an inbound datagram goes; `None` drops it.
    fn route(&mut self, bytes: &[u8], from: SocketAddr) -> Option<Route> {
        // The version probe (spec 120 § 12.2, ADR-7985) goes to the layer the source already
        // talks to, else to the game's link: its endpoint answers with the game's versions.
        if crate::version_probe::is_probe_request(bytes) {
            let layer = self
                .routes
                .get(&from)
                .or_else(|| self.pending.get(&from))
                .copied()
                .unwrap_or(PortLayer::Link);
            return Some(Route::One(layer));
        }
        let Some(kind) = kind_of(bytes) else {
            self.stats.dropped_foreign += 1;
            return None;
        };
        if kind == PacketKind::ConnectRequest {
            let layer = match ConnectRequest::decode(&bytes[5..]) {
                Ok(req) if req.protocol_id == self.link_protocol => PortLayer::Link,
                _ => PortLayer::Session,
            };
            return Some(Route::One(layer));
        }
        // A `ResumeInit` stands in for a `ConnectRequest` (ADR-8858): the same stateless rule, on
        // the protocol id that opens its body. A resume from a new address has no route yet, and
        // the session half would answer the link's flight with a protocol deny.
        if kind == PacketKind::ResumeInit {
            let id = bytes
                .get(5..13)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_le_bytes);
            let layer = if id == Some(self.link_protocol) {
                PortLayer::Link
            } else {
                PortLayer::Session
            };
            return Some(Route::One(layer));
        }
        // A handshake reply follows its challenge, even past an older route: the client may be
        // rejoining from the same address on the other layer. Connected traffic never reads
        // `pending`, so a spoofed request cannot move a live peer. With no row (never one, or
        // pushed out by forged requests) both halves check the cookie; a cookie the half did not
        // mint is dropped there without an answer.
        if matches!(kind, PacketKind::KeyInit | PacketKind::ChallengeResponse) {
            return Some(self.pending.get(&from).map_or(Route::Both, |l| Route::One(*l)));
        }
        if kind == PacketKind::Sealed
            && let Some(layer) = SealedHeader::parse(&bytes[5..]).and_then(|h| self.tokens.get(&h.token))
        {
            return Some(Route::One(*layer));
        }
        if let Some(layer) = self.routes.get(&from) {
            return Some(Route::One(*layer));
        }
        Some(Route::One(
            self.pending.get(&from).copied().unwrap_or(PortLayer::Session),
        ))
    }

    fn enqueue(&mut self, layer: PortLayer, bytes: &[u8], from: SocketAddr) {
        let i = layer.index();
        if self.queues[i].len() >= MAX_QUEUED_DATAGRAMS
            || self.stats.queued_bytes[i] + bytes.len() > MAX_QUEUED_BYTES
        {
            self.stats.dropped_full += 1;
            self.note_drop(from);
            return;
        }
        self.queues[i].push_back((bytes.to_vec(), from));
        self.stats.queued_bytes[i] += bytes.len();
        self.stats.routed[i] += 1;
    }

    /// Counts one dropped datagram against its source.
    fn note_drop(&mut self, from: SocketAddr) {
        if let Some(n) = self.drops.get_mut(&from) {
            *n += 1;
        } else {
            remember(&mut self.drops, &mut self.drop_order, MAX_DROP_SOURCES, from, 1);
        }
    }

    /// Reads what is waiting on the socket into the queues, within `reader`'s reads for this round.
    fn pump(&mut self, reader: PortLayer) -> io::Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(READ_BUFFER, 0);
        let result = loop {
            if self.reads_left[reader.index()] == 0 {
                break Ok(());
            }
            self.reads_left[reader.index()] -= 1;
            match self.socket.recv_from(&mut buf) {
                Ok(Some((n, from))) if n > MAX_DATAGRAM_BYTES => {
                    // No endpoint reads past `MAX_DATAGRAM_BYTES`: never copied (spec 120 § 11.5).
                    self.stats.dropped_oversize += 1;
                    self.note_drop(from);
                }
                Ok(Some((n, from))) => {
                    let bytes = &buf[..n];
                    match self.route(bytes, from) {
                        Some(Route::One(layer)) => self.enqueue(layer, bytes, from),
                        Some(Route::Both) => {
                            self.enqueue(PortLayer::Session, bytes, from);
                            self.enqueue(PortLayer::Link, bytes, from);
                        }
                        None => {}
                    }
                }
                Ok(None) => break Ok(()),
                // A UDP port reports an ICMP error from an earlier send as a receive error on
                // some platforms; the half's endpoint reads it as before.
                Err(e) => break Err(e),
            }
        };
        self.buf = buf;
        result
    }

    /// Notes what a half sends: a challenge marks the address pending for it, a datagram that
    /// follows a checked cookie claims the address.
    fn note_send(&mut self, layer: PortLayer, bytes: &[u8], dest: SocketAddr) {
        match kind_of(bytes) {
            Some(PacketKind::Challenge) => remember(
                &mut self.pending,
                &mut self.pending_order,
                MAX_PENDING_CHALLENGES,
                dest,
                layer,
            ),
            Some(
                kind @ (PacketKind::Accept
                | PacketKind::KeyReply
                | PacketKind::Payload
                | PacketKind::Sealed
                | PacketKind::ResumeReply),
            ) => {
                if self.pending.remove(&dest).is_some() {
                    // Rare (one per handshake), and bounded by the table's size.
                    self.pending_order.retain(|a| *a != dest);
                }
                // A route that changes layer is overwritten in place.
                remember(&mut self.routes, &mut self.route_order, MAX_ROUTES, dest, layer);
                self.stats.routes = self.routes.len();
                // A `ResumeReply` follows a proved PSK; the sealed accept after its Noise message
                // carries the new link's token in the clear, like any record.
                let record = match kind {
                    PacketKind::Sealed => bytes.get(5..),
                    PacketKind::ResumeReply => bytes
                        .get(5 + 8 + crate::crypto::RESUME_REPLY_NOISE_BYTES..)
                        .filter(|sealed| kind_of(sealed) == Some(PacketKind::Sealed))
                        .and_then(|sealed| sealed.get(5..)),
                    _ => None,
                };
                if let Some(header) = record.and_then(SealedHeader::parse) {
                    remember(
                        &mut self.tokens,
                        &mut self.token_order,
                        MAX_TOKENS,
                        header.token,
                        layer,
                    );
                }
            }
            _ => {}
        }
    }
}

/// One socket shared by the session and the link (see the module docs).
#[derive(Clone)]
pub struct SharedPort {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for SharedPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedPort")
            .field("stats", &self.stats())
            .finish()
    }
}

/// One layer's half of a [`SharedPort`]: a [`DatagramSocket`] for its endpoint.
#[derive(Debug)]
pub struct PortHalf {
    port: SharedPort,
    layer: PortLayer,
}

impl SharedPort {
    /// Splits `socket` into the session half and the link half. `link_protocol` is the link
    /// game's protocol id (`crate::payload::protocol_id::<G>()`).
    pub fn split(socket: Box<dyn DatagramSocket + Send>, link_protocol: u64) -> (Self, PortHalf, PortHalf) {
        let port = Self {
            inner: Arc::new(Mutex::new(Inner {
                socket,
                link_protocol,
                queues: [VecDeque::new(), VecDeque::new()],
                routes: HashMap::new(),
                route_order: VecDeque::new(),
                pending: HashMap::new(),
                pending_order: VecDeque::new(),
                tokens: HashMap::new(),
                token_order: VecDeque::new(),
                reads_left: [MAX_READS_PER_ROUND; 2],
                stats: SharedPortStats::default(),
                drops: HashMap::new(),
                drop_order: VecDeque::new(),
                buf: Vec::new(),
            })),
        };
        let session = PortHalf {
            port: port.clone(),
            layer: PortLayer::Session,
        };
        let link = PortHalf {
            port: port.clone(),
            layer: PortLayer::Link,
        };
        (port, session, link)
    }

    /// The counters.
    pub fn stats(&self) -> SharedPortStats {
        self.lock().stats
    }

    /// Datagrams dropped per source (a full queue or an oversize datagram), most first; at most
    /// [`MAX_DROP_SOURCES`] sources, the oldest forgotten first.
    pub fn drops_by_source(&self) -> Vec<(SocketAddr, u64)> {
        let mut out: Vec<(SocketAddr, u64)> = self.lock().drops.iter().map(|(a, n)| (*a, *n)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// The layer an address is routed to, if it completed a handshake.
    pub fn route_of(&self, addr: SocketAddr) -> Option<PortLayer> {
        self.lock().routes.get(&addr).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PortHalf {
    /// The layer this half serves.
    pub fn layer(&self) -> PortLayer {
        self.layer
    }
}

impl DatagramSocket for PortHalf {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let mut inner = self.port.lock();
        inner.note_send(self.layer, bytes, dest);
        inner.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut inner = self.port.lock();
        if inner.queues[self.layer.index()].is_empty() {
            inner.pump(self.layer)?;
        }
        let Some((bytes, from)) = inner.queues[self.layer.index()].pop_front() else {
            // The endpoint's receive ends here: the next one starts a new round of reads.
            inner.reads_left[self.layer.index()] = MAX_READS_PER_ROUND;
            return Ok(None);
        };
        let queued = &mut inner.stats.queued_bytes[self.layer.index()];
        *queued = queued.saturating_sub(bytes.len());
        // A datagram longer than the caller's buffer is cut, as a UDP socket would.
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.port.lock().socket.local_addr()
    }

    fn socket_report(&self) -> Option<crate::socket::SocketReport> {
        self.port.lock().socket.socket_report()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::ConnectRequest;
    use crate::socket::LoopbackNet;

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("address")
    }

    fn request(protocol_id: u64) -> Vec<u8> {
        ConnectRequest {
            protocol_id,
            client_nonce: 7,
            crypto_version: 0,
            crypto_required: false,
        }
        .to_datagram()
    }

    fn header(kind: PacketKind) -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.push(kind as u8);
        v.extend_from_slice(&[0; 8]);
        v
    }

    fn recv(half: &mut PortHalf) -> Option<SocketAddr> {
        let mut buf = [0u8; 2048];
        half.recv_from(&mut buf).expect("recv").map(|(_, from)| from)
    }

    /// The version probe (spec 120 § 12.2, ADR-7985) is not dropped as foreign: a stranger's goes
    /// to the link half (the game's server answers it), a session peer's to the session half.
    #[test]
    fn version_probes_reach_a_layer_that_answers_them() {
        let net = LoopbackNet::new();
        let server = addr("10.9.0.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let probe = crate::version_probe::ProbeRequest { nonce: 3 }.to_datagram();
        let mut stranger = net.bind(addr("10.9.0.2:1")).expect("bind");
        stranger.send_to(&probe, server).expect("send");
        assert_eq!(recv(&mut link), Some(addr("10.9.0.2:1")));
        assert_eq!(recv(&mut session), None);
        assert_eq!(port.stats().dropped_foreign, 0);
        session
            .send_to(&header(PacketKind::Accept), addr("10.9.0.3:1"))
            .expect("send");
        let mut peer = net.bind(addr("10.9.0.3:1")).expect("bind");
        peer.send_to(&probe, server).expect("send");
        assert_eq!(recv(&mut session), Some(addr("10.9.0.3:1")));
        assert_eq!(recv(&mut link), None);
    }

    /// Wire 6 (ADR-8858): a `ResumeInit` stands in for a `ConnectRequest`, so it routes by its
    /// protocol id from an address with no route, and the `ResumeReply` that answers it claims the
    /// address and the token of the sealed accept it carries.
    #[test]
    fn a_resume_init_routes_by_protocol_and_its_reply_claims_the_address_and_token() {
        let net = LoopbackNet::new();
        let server = addr("10.9.5.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let init = |protocol_id: u64| {
            crate::crypto::ResumeInit {
                protocol_id,
                client_nonce: 7,
                crypto_version: 2,
                cookie: None,
                ticket: [0; crate::handshake::RESUMPTION_TICKET_BYTES],
                noise: vec![0; 64],
            }
            .to_datagram()
            .expect("datagram")
        };
        let mut a = net.bind(addr("10.9.5.2:1")).expect("bind");
        let mut b = net.bind(addr("10.9.5.3:1")).expect("bind");
        a.send_to(&init(42), server).expect("send");
        b.send_to(&init(5), server).expect("send");
        assert_eq!(recv(&mut link), Some(addr("10.9.5.2:1")), "the link's protocol");
        assert_eq!(recv(&mut link), None);
        assert_eq!(recv(&mut session), Some(addr("10.9.5.3:1")), "any other protocol");
        assert_eq!(port.route_of(addr("10.9.5.2:1")), None, "a flight claims nothing");
        // The reply: nonce, the Noise message, then a sealed record under token 0x77.
        let mut sealed = [0u8; crate::crypto::SEALED_HEADER_BYTES];
        SealedHeader::write(&mut sealed, 0x77, 0);
        let mut record = sealed.to_vec();
        record.extend_from_slice(&[0; 40]);
        let reply = crate::crypto::ResumeReply {
            client_nonce: 7,
            noise: [0; crate::crypto::RESUME_REPLY_NOISE_BYTES],
            accept: record.clone(),
        }
        .to_datagram()
        .expect("datagram");
        link.send_to(&reply, addr("10.9.5.2:1")).expect("send");
        assert_eq!(port.route_of(addr("10.9.5.2:1")), Some(PortLayer::Link));
        // The same token from another address (the client's NAT moved it) still finds the link.
        let mut moved = net.bind(addr("10.9.5.9:1")).expect("bind");
        moved.send_to(&record, server).expect("send");
        assert_eq!(recv(&mut link), Some(addr("10.9.5.9:1")));
        assert_eq!(recv(&mut session), None);
    }

    #[test]
    fn requests_route_by_protocol_and_traffic_by_the_claimed_address() {
        let net = LoopbackNet::new();
        let server = addr("10.9.0.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let mut a = net.bind(addr("10.9.0.2:1")).expect("bind");
        let mut b = net.bind(addr("10.9.0.3:1")).expect("bind");
        a.send_to(&request(42), server).expect("send");
        b.send_to(&request(5), server).expect("send");
        // Foreign bytes are dropped and counted.
        b.send_to(b"not regia", server).expect("send");
        assert_eq!(recv(&mut link), Some(addr("10.9.0.2:1")), "the link's protocol");
        assert_eq!(recv(&mut link), None);
        assert_eq!(recv(&mut session), Some(addr("10.9.0.3:1")), "any other protocol");
        assert_eq!(recv(&mut session), None);
        assert_eq!(port.stats().dropped_foreign, 1);
        // Before a claim, a challenge reply goes to the half that sent the challenge.
        link.send_to(&header(PacketKind::Challenge), addr("10.9.0.2:1"))
            .expect("send");
        a.send_to(&header(PacketKind::ChallengeResponse), server)
            .expect("send");
        assert_eq!(recv(&mut link), Some(addr("10.9.0.2:1")));
        assert_eq!(
            port.route_of(addr("10.9.0.2:1")),
            None,
            "a challenge claims nothing"
        );
        // An accept claims the address; its traffic follows.
        link.send_to(&header(PacketKind::Accept), addr("10.9.0.2:1"))
            .expect("send");
        assert_eq!(port.route_of(addr("10.9.0.2:1")), Some(PortLayer::Link));
        a.send_to(&header(PacketKind::Payload), server).expect("send");
        assert_eq!(recv(&mut session), None);
        assert_eq!(recv(&mut link), Some(addr("10.9.0.2:1")));
        // An address with no route or challenge goes to the session (it drops what it does not know).
        let mut c = net.bind(addr("10.9.0.4:1")).expect("bind");
        c.send_to(&header(PacketKind::Payload), server).expect("send");
        assert_eq!(recv(&mut session), Some(addr("10.9.0.4:1")));
        assert_eq!(port.stats().routes, 1);
    }

    #[test]
    fn a_rejoin_on_the_other_layer_follows_its_challenge_and_a_spoof_moves_no_traffic() {
        let net = LoopbackNet::new();
        let server = addr("10.9.2.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let from = addr("10.9.2.2:1");
        let mut a = net.bind(from).expect("bind");
        link.send_to(&header(PacketKind::Accept), from).expect("send");
        assert_eq!(port.route_of(from), Some(PortLayer::Link));

        // The same address asks the session to admit it: every handshake reply follows the
        // session's challenge, not the older route.
        a.send_to(&request(5), server).expect("send");
        assert_eq!(recv(&mut session), Some(from));
        session
            .send_to(&header(PacketKind::Challenge), from)
            .expect("send");
        for reply in [PacketKind::KeyInit, PacketKind::ChallengeResponse] {
            a.send_to(&header(reply), server).expect("send");
            assert_eq!(recv(&mut session), Some(from), "{reply:?} follows the challenge");
            assert_eq!(recv(&mut link), None);
        }

        // That request could be spoofed: connected traffic stays on the route until the session
        // really admits the address.
        a.send_to(&header(PacketKind::Sealed), server).expect("send");
        assert_eq!(recv(&mut link), Some(from), "connected traffic keeps its route");
        assert_eq!(recv(&mut session), None);

        // Once admitted, connected traffic moves too.
        session.send_to(&header(PacketKind::Accept), from).expect("send");
        assert_eq!(port.route_of(from), Some(PortLayer::Session));
        a.send_to(&header(PacketKind::Sealed), server).expect("send");
        assert_eq!(recv(&mut session), Some(from));
        assert_eq!(recv(&mut link), None);
    }

    /// A record as `SealedHeader` writes it: token, counter, then ciphertext and tag.
    fn sealed(token: u32) -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.push(PacketKind::Sealed as u8);
        v.extend_from_slice(&token.to_le_bytes());
        v.extend_from_slice(&[0; 8 + 16]);
        v
    }

    #[test]
    fn a_rebound_peer_reaches_the_layer_of_its_session_token() {
        let net = LoopbackNet::new();
        let server = addr("10.9.3.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let old = addr("10.9.3.2:1");
        link.send_to(&sealed(0xAAAA), old).expect("send");
        assert_eq!(port.route_of(old), Some(PortLayer::Link));

        // The NAT moved the link's peer: an address with no route, carrying the link's token.
        let moved = addr("10.9.3.3:1");
        let mut m = net.bind(moved).expect("bind");
        m.send_to(&sealed(0xAAAA), server).expect("send");
        assert_eq!(recv(&mut session), None, "the session could not open it");
        assert_eq!(recv(&mut link), Some(moved));
        assert_eq!(
            port.route_of(moved),
            None,
            "moving claims nothing until the link answers"
        );

        // A session peer moved onto an address the link used before: the token wins over the
        // stale route, so the session can authenticate it.
        session
            .send_to(&sealed(0xBBBB), addr("10.9.3.4:1"))
            .expect("send");
        let mut stale = net.bind(old).expect("bind");
        stale.send_to(&sealed(0xBBBB), server).expect("send");
        assert_eq!(recv(&mut link), None);
        assert_eq!(recv(&mut session), Some(old));

        // A token nobody sends under keeps the address's route.
        stale.send_to(&sealed(0xCCCC), server).expect("send");
        assert_eq!(recv(&mut session), None);
        assert_eq!(recv(&mut link), Some(old));
    }

    #[test]
    fn forged_requests_cannot_starve_a_real_handshake_on_the_other_layer() {
        let net = LoopbackNet::new();
        let server = addr("10.9.4.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let joiner = addr("10.9.4.2:1");
        let mut j = net.bind(joiner).expect("bind");
        link.send_to(&header(PacketKind::Challenge), joiner)
            .expect("send");

        // Forged sources: every one is answered with a session challenge, and pushes the joiner's
        // row out of the table.
        for i in 0..(MAX_PENDING_CHALLENGES + 1) {
            let to = SocketAddr::from(([10, 7, (i / 256) as u8, (i % 256) as u8], 9));
            session.send_to(&header(PacketKind::Challenge), to).expect("send");
        }
        assert!(!port.lock().pending.contains_key(&joiner), "pushed out");

        // The reply still reaches the link, which minted its cookie; the session also sees it
        // and drops a cookie it did not mint.
        j.send_to(&header(PacketKind::KeyInit), server).expect("send");
        assert_eq!(recv(&mut link), Some(joiner));
        assert_eq!(recv(&mut session), Some(joiner));
        assert_eq!(port.stats().routed, [1, 1]);
    }

    #[test]
    fn the_pending_table_keeps_no_row_after_a_handshake_finishes() {
        let net = LoopbackNet::new();
        let server = addr("10.9.5.1:7000");
        let (port, _session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let to = addr("10.9.5.2:1");
        // One address repeating the handshake never reaches the table's bound, so nothing else
        // would trim the order queue.
        for _ in 0..(MAX_PENDING_CHALLENGES * 3) {
            link.send_to(&header(PacketKind::Challenge), to).expect("send");
            link.send_to(&header(PacketKind::Accept), to).expect("send");
        }
        let inner = port.lock();
        assert!(inner.pending.is_empty());
        assert!(inner.pending_order.is_empty(), "{}", inner.pending_order.len());
    }

    #[test]
    fn a_flood_cannot_hold_one_receive_past_its_reads() {
        let net = LoopbackNet::new();
        let server = addr("10.9.6.1:7000");
        let (port, mut session, _link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let mut flood = net.bind(addr("10.9.6.2:1")).expect("bind");
        let extra = 5;
        for _ in 0..(MAX_READS_PER_ROUND + extra) {
            flood.send_to(b"not regia", server).expect("send");
        }
        assert_eq!(recv(&mut session), None);
        assert_eq!(
            port.stats().dropped_foreign,
            MAX_READS_PER_ROUND as u64,
            "one round reads its budget, even with more waiting"
        );
        assert_eq!(recv(&mut session), None);
        assert_eq!(
            port.stats().dropped_foreign,
            (MAX_READS_PER_ROUND + extra) as u64,
            "the next round reads the rest"
        );
    }

    #[test]
    fn queues_and_tables_are_bounded() {
        let net = LoopbackNet::new();
        let server = addr("10.9.1.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let mut flood = net.bind(addr("10.9.1.2:1")).expect("bind");
        for _ in 0..(MAX_QUEUED_DATAGRAMS + 10) {
            flood.send_to(&request(42), server).expect("send");
        }
        // Reading the session half pumps everything; the link's queue keeps its bound.
        assert_eq!(recv(&mut session), None);
        assert_eq!(port.stats().dropped_full, 10);
        let mut n = 0;
        while recv(&mut link).is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_QUEUED_DATAGRAMS);
        // Claims past the bound evict the oldest.
        for i in 0..(MAX_ROUTES + 3) {
            let to = SocketAddr::from(([10, 8, (i / 256) as u8, (i % 256) as u8], 9));
            link.send_to(&header(PacketKind::Accept), to).expect("send");
        }
        assert_eq!(port.stats().routes, MAX_ROUTES);
        assert_eq!(
            port.route_of(SocketAddr::from(([10, 8, 0, 0], 9))),
            None,
            "evicted"
        );
    }

    /// Spec 120 § 11.5 / adversarial row 14 (ADR-7981): a datagram longer than any endpoint reads
    /// is dropped without a copy and counted against its source, and a half's queue holds at most
    /// `MAX_QUEUED_BYTES`, so a flood of full-size datagrams holds megabytes, not 256 MiB.
    #[test]
    fn queued_bytes_are_bounded_and_drops_are_counted_per_source() {
        let net = LoopbackNet::new();
        let server = addr("10.9.7.1:7000");
        let (port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
        let big_source = addr("10.9.7.2:1");
        let mut big = net.bind(big_source).expect("bind");
        // Unrouted payloads go to the session half.
        let mut oversize = header(PacketKind::Payload);
        oversize.resize(65_000, 0);
        for _ in 0..10 {
            big.send_to(&oversize, server).expect("send");
        }
        let mut full = header(PacketKind::Payload);
        full.resize(MAX_DATAGRAM_BYTES, 0);
        let full_source = addr("10.9.7.3:1");
        let mut flood = net.bind(full_source).expect("bind");
        for _ in 0..MAX_QUEUED_DATAGRAMS {
            flood.send_to(&full, server).expect("send");
        }
        assert_eq!(recv(&mut link), None, "reading one half pumps both");
        let stats = port.stats();
        assert_eq!(stats.dropped_oversize, 10, "never queued");
        assert!(stats.queued_bytes[0] <= MAX_QUEUED_BYTES, "{stats:?}");
        let queued = MAX_QUEUED_BYTES / MAX_DATAGRAM_BYTES;
        assert_eq!(stats.queued_bytes[0], queued * MAX_DATAGRAM_BYTES);
        assert_eq!(stats.dropped_full, (MAX_QUEUED_DATAGRAMS - queued) as u64);
        assert_eq!(
            port.drops_by_source(),
            vec![
                (full_source, (MAX_QUEUED_DATAGRAMS - queued) as u64),
                (big_source, 10)
            ]
        );
        let mut n = 0;
        while recv(&mut session).is_some() {
            n += 1;
        }
        assert_eq!(n, queued);
        assert_eq!(port.stats().queued_bytes, [0, 0], "reading frees the bytes");
    }
}
