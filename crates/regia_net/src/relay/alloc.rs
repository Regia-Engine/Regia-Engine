// SPDX-License-Identifier: MPL-2.0
//! Relay protocol 2 allocations (spec 120 § 10.6, ADR-7982): one relay port per relayed client.
//!
//! The shape is TURN's (RFC 8656 § 5): a client's `Allocate` reserves a port of the relay's pool
//! for that client alone, so the host sees each relayed client as its own source address
//! (`relay_ip:port`) and the record layer, the peer table and rebinding above the socket need no
//! change. An allocation has two permissions (RFC 8656 § 9): the client address and the host
//! address. A datagram to the port from either is forwarded to the other without parsing; one
//! from any other source is dropped and counted, so a forger who learns a port can neither spend
//! its budget nor block it. A side whose address changed (a NAT rebind, or a host whose game
//! socket is not its registered one) moves its permission with a `Refresh` that carries its own
//! key (checked in constant time) and proves the new source with a cookie round trip.
//!
//! The code here is original; the design follows the public RFC 8656.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use super::{
    Frame, RELAY_ALLOCATE_REQUEST_BYTES, RELAY_ALLOCATION_KEY_BYTES, RELAY_BIND_KEY_BYTES,
    RELAY_BIND_REPLACE_IDLE_MS, RELAY_COOKIE_BYTES, RELAY_FLOW_IDLE_MS, RELAY_FORWARD_BYTES_PER_SECOND,
    RELAY_HOST_BYTES_PER_SECOND, RELAY_MAX_ALLOCATIONS, RELAY_MAX_ALLOCATIONS_PER_IP,
    RELAY_MAX_CLIENTS_PER_HOST, RELAY_PROTOCOL_VERSION, RELAY_REFRESH_REQUEST_BYTES, RelayCore, RelayError,
    RelayTag, control_v, frame,
};
use crate::addr_block::AddrBlock;
use crate::handshake::{decode_addr, encode_addr};
use crate::wire::{WireReader, constant_time_eq};

/// Cookie context of an `Allocate` (a `Register` uses 0).
const COOKIE_ALLOCATE: u64 = 1;
/// Cookie context of a `Refresh`; the allocation port is mixed in above it.
const COOKIE_REFRESH: u64 = 2 << 16;

/// Operator settings of a relay's allocations (spec 120 § 10.6). Every cap answers
/// [`super::RelayReply::Full`] with its value when reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Allocation ports [`super::RelayServer::bind`] opens (the pool, default 4,096). Each is
    /// one UDP socket, so a relay this large needs a file-descriptor limit above it.
    pub allocation_ports: u16,
    /// First allocation port; `0` lets the system pick each one.
    pub allocation_port_base: u16,
    /// Relayed clients one host may have at once (default 40: DO27 runs 16 per player host and
    /// 32, later 40, per community dedicated server).
    pub max_clients_per_host: u32,
    /// Allocations one source IP (or IPv6 /64) may hold at once (default 8).
    pub max_allocations_per_ip: u32,
    /// Bytes per second one direction of one allocation forwards, and its burst (default
    /// 4 MiB/s, the protocol 1 flow rate).
    pub allocation_bytes_per_second: u64,
    /// Bytes per second one host's allocations forward together, both directions (default
    /// 64 MiB/s).
    pub host_bytes_per_second: u64,
    /// Bytes per second every allocation of the relay forwards together; `0` is no cap.
    pub relay_bytes_per_second: u64,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            allocation_ports: RELAY_MAX_ALLOCATIONS as u16,
            allocation_port_base: 0,
            max_clients_per_host: RELAY_MAX_CLIENTS_PER_HOST,
            max_allocations_per_ip: RELAY_MAX_ALLOCATIONS_PER_IP,
            allocation_bytes_per_second: RELAY_FORWARD_BYTES_PER_SECOND,
            host_bytes_per_second: RELAY_HOST_BYTES_PER_SECOND,
            relay_bytes_per_second: 0,
        }
    }
}

/// What a relay holds and what it refused, for operators and agents (`net.relay_status`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    /// The relay control protocol this relay speaks.
    pub protocol: u8,
    /// Lobbies registered.
    pub lobbies: usize,
    /// Protocol 1 flows (one relayed client per host address).
    pub flows: usize,
    /// Directory listings.
    pub listings: usize,
    /// Protocol 2 allocations held.
    pub allocations: usize,
    /// Allocation ports in the pool.
    pub ports: usize,
    /// Allocation ports free.
    pub ports_free: usize,
    /// Hosts with at least one allocation.
    pub hosts: usize,
    /// Most allocations any one host holds.
    pub most_clients_of_one_host: u32,
    /// The per-host cap.
    pub max_clients_per_host: u32,
    /// Datagrams forwarded (flows and allocations).
    pub forwarded: u64,
    /// Datagrams a flow or an allocation dropped for going over its own rate.
    pub forward_dropped: u64,
    /// Datagrams dropped because the host's allocations were over the per-host rate.
    pub host_rate_dropped: u64,
    /// Datagrams dropped because the relay was over its total allocation rate.
    pub relay_rate_dropped: u64,
    /// Datagrams to an allocation port from an address with no permission there.
    pub permission_dropped: u64,
    /// `Refresh` requests whose key matched neither side.
    pub refresh_refused: u64,
    /// Permissions moved to a new address by a `Refresh` or a returning `Allocate`.
    pub refreshed: u64,
    /// `Allocate` requests refused by a cap.
    pub full_refused: u64,
    /// Requests answered `VersionTooOld` or `VersionTooNew`.
    pub version_refused: u64,
    /// Control requests dropped by the per-source limit.
    pub rate_limited: u64,
}

/// Token bucket of one rate (`0` is no limit).
#[derive(Debug, Clone, Copy)]
struct Bucket {
    credit: u64,
    at_ms: u64,
}

impl Bucket {
    fn new(rate: u64, now_ms: u64) -> Self {
        Self {
            credit: rate,
            at_ms: now_ms,
        }
    }

    /// Refills to `now_ms`; true when `bytes` fit.
    fn refill(&mut self, rate: u64, bytes: u64, now_ms: u64) -> bool {
        if rate == 0 {
            return true;
        }
        let gained = now_ms.saturating_sub(self.at_ms).saturating_mul(rate) / 1000;
        self.credit = self.credit.saturating_add(gained).min(rate);
        self.at_ms = now_ms;
        self.credit >= bytes
    }

    fn take(&mut self, rate: u64, bytes: u64) {
        if rate != 0 {
            self.credit = self.credit.saturating_sub(bytes);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Client,
    Host,
}

/// One client's allocation.
#[derive(Debug, Clone)]
struct Allocation {
    /// The host's registered address (the lobby it belongs to).
    lobby_host: SocketAddr,
    /// Permitted host address (moves with the host key).
    host: SocketAddr,
    /// Permitted client address (moves with the client key).
    client: SocketAddr,
    client_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    host_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    /// The source block charged for this allocation.
    block: AddrBlock,
    to_host: Bucket,
    to_client: Bucket,
    client_sent_ms: u64,
    host_sent_ms: u64,
    /// Bytes forwarded client to host, and host to client (`net.relay_status`).
    bytes_to_host: u64,
    bytes_to_client: u64,
}

/// A relay as an agent sees it (`net.relay_status`, spec 120 § 10.6, ADR-8183): a relay this
/// process runs (`role` `"relay"`), or a host's registration with a relay (`role` `"host"`), or
/// both in one report for a host with `--relay local`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayReport {
    /// `"relay"`, `"host"` or `"host+relay"`.
    pub role: String,
    /// The relay's address.
    pub relay: String,
    /// The host's join code, once the relay gave one.
    pub join_code: Option<String>,
    /// Relayed clients the relay announced to the host (`ClientJoined`).
    pub relayed_clients: u64,
    /// Of those, allocations whose host side the relay confirmed.
    pub confirmed_allocations: u64,
    /// The relay's counters, when this process runs it.
    pub status: Option<RelayStatus>,
    /// The relay's caps, when this process runs it.
    pub config: Option<RelayConfig>,
    /// Each allocation and the bytes it forwarded, when this process runs the relay.
    pub allocations: Vec<RelayAllocationView>,
    /// A host's NAT traversal (spec 120 § 10.7, ADR-8340): its candidates, introductions and
    /// checks; `None` for a relay or a registrar off the game socket.
    #[serde(default)]
    pub nat: Option<crate::nat::NatReport>,
    /// A host's check before hosting (spec 120 § 10.7, ADR-8370): its port mapping, the
    /// port-forward self-check and the capacity estimate (`net.host_check`); `None` for a relay or
    /// a host that did not run it.
    #[serde(default)]
    pub host_check: Option<crate::host_check::HostCheckReport>,
}

/// One allocation as an operator or agent sees it (`net.relay_status`, spec 120 § 10.6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayAllocationView {
    /// The allocation's port on the relay.
    pub port: u16,
    /// The host's registered address (its lobby).
    pub host: String,
    /// The client's permitted address.
    pub client: String,
    /// Bytes forwarded from the client to the host.
    pub bytes_to_host: u64,
    /// Bytes forwarded from the host to the client.
    pub bytes_to_client: u64,
}

/// One host's share of the relay.
#[derive(Debug, Clone, Copy)]
struct HostShare {
    clients: u32,
    bytes: Bucket,
}

/// The allocation tables of a [`RelayCore`].
#[derive(Debug)]
pub(super) struct Allocations {
    config: RelayConfig,
    table: HashMap<u16, Allocation>,
    /// `(registered host, client key) -> port`.
    by_client: HashMap<(SocketAddr, [u8; RELAY_ALLOCATION_KEY_BYTES]), u16>,
    hosts: HashMap<SocketAddr, HostShare>,
    per_ip: HashMap<AddrBlock, u32>,
    /// Free ports, oldest release first, so a port is reused as late as possible.
    free: VecDeque<u16>,
    known: HashSet<u16>,
    global: Bucket,
    key_secret: [u8; 32],
    next_key: u64,
    pub(super) status: RelayStatus,
}

impl Allocations {
    pub(super) fn new(config: RelayConfig, secret: &[u8; 32]) -> Self {
        let global = Bucket::new(config.relay_bytes_per_second, 0);
        Self {
            config,
            table: HashMap::new(),
            by_client: HashMap::new(),
            hosts: HashMap::new(),
            per_ip: HashMap::new(),
            free: VecDeque::new(),
            known: HashSet::new(),
            global,
            key_secret: blake3::derive_key("regia_net relay host key v2", secret),
            next_key: 0,
            status: RelayStatus::default(),
        }
    }

    pub(super) fn config(&self) -> &RelayConfig {
        &self.config
    }

    fn host_key(&mut self, port: u16) -> [u8; RELAY_ALLOCATION_KEY_BYTES] {
        let mut input = [0u8; 10];
        input[..8].copy_from_slice(&self.next_key.to_le_bytes());
        input[8..].copy_from_slice(&port.to_le_bytes());
        self.next_key = self.next_key.wrapping_add(1);
        let hash = blake3::keyed_hash(&self.key_secret, &input);
        let mut key = [0u8; RELAY_ALLOCATION_KEY_BYTES];
        key.copy_from_slice(&hash.as_bytes()[..RELAY_ALLOCATION_KEY_BYTES]);
        key
    }

    fn release(&mut self, port: u16) {
        let Some(a) = self.table.remove(&port) else {
            return;
        };
        self.by_client.remove(&(a.lobby_host, a.client_key));
        if let Some(share) = self.hosts.get_mut(&a.lobby_host) {
            share.clients = share.clients.saturating_sub(1);
            if share.clients == 0 {
                self.hosts.remove(&a.lobby_host);
            }
        }
        if let Some(n) = self.per_ip.get_mut(&a.block) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_ip.remove(&a.block);
            }
        }
        self.free.push_back(port);
    }

    /// Drops allocations whose two sides were both quiet for [`RELAY_FLOW_IDLE_MS`].
    pub(super) fn expire(&mut self, now_ms: u64) {
        let dead: Vec<u16> = self
            .table
            .iter()
            .filter(|(_, a)| {
                now_ms.saturating_sub(a.client_sent_ms.max(a.host_sent_ms)) >= RELAY_FLOW_IDLE_MS
            })
            .map(|(port, _)| *port)
            .collect();
        for port in dead {
            self.release(port);
        }
    }
}

fn allocated(port: u16, peer: SocketAddr, to: SocketAddr) -> (Vec<u8>, SocketAddr) {
    let mut w = control_v(RelayTag::Allocated, RELAY_PROTOCOL_VERSION);
    w.u16(port);
    encode_addr(&mut w, peer);
    (w.finish(), to)
}

fn challenge(cookie: [u8; RELAY_COOKIE_BYTES], to: SocketAddr) -> Vec<(Vec<u8>, SocketAddr)> {
    let mut w = control_v(RelayTag::Challenge, RELAY_PROTOCOL_VERSION);
    w.raw(&cookie);
    vec![(w.finish(), to)]
}

fn full(cap: u32, to: SocketAddr) -> Vec<(Vec<u8>, SocketAddr)> {
    let mut w = control_v(RelayTag::AllocationFull, RELAY_PROTOCOL_VERSION);
    w.u16(u16::try_from(cap).unwrap_or(u16::MAX));
    vec![(w.finish(), to)]
}

fn refusal(code: RelayError, to: SocketAddr) -> Vec<(Vec<u8>, SocketAddr)> {
    let mut w = control_v(RelayTag::Error, RELAY_PROTOCOL_VERSION);
    w.u8(code as u8);
    vec![(w.finish(), to)]
}

/// An optional cookie: `0`, or `1` and the cookie.
fn read_cookie(r: &mut WireReader<'_>) -> Option<[u8; RELAY_COOKIE_BYTES]> {
    match r.u8() {
        Ok(1) => r.array::<RELAY_COOKIE_BYTES>().ok(),
        _ => None,
    }
}

impl RelayCore {
    /// Adds allocation ports to the pool: ports the caller listens on and passes to
    /// [`RelayCore::handle_allocation`]. [`super::RelayServer::bind`] does this for its sockets.
    pub fn add_allocation_ports(&mut self, ports: impl IntoIterator<Item = u16>) {
        for port in ports {
            if self.alloc.known.insert(port) {
                self.alloc.free.push_back(port);
            }
        }
    }

    /// The operator settings of this relay's allocations.
    pub fn relay_config(&self) -> &RelayConfig {
        self.alloc.config()
    }

    /// Ports with an allocation, in no particular order.
    pub fn live_allocation_ports(&self) -> Vec<u16> {
        self.alloc.table.keys().copied().collect()
    }

    /// Every allocation with its addresses and the bytes it forwarded each way, by port.
    pub fn allocations(&self) -> Vec<RelayAllocationView> {
        let mut out: Vec<RelayAllocationView> = self
            .alloc
            .table
            .iter()
            .map(|(port, a)| RelayAllocationView {
                port: *port,
                host: a.lobby_host.to_string(),
                client: a.client.to_string(),
                bytes_to_host: a.bytes_to_host,
                bytes_to_client: a.bytes_to_client,
            })
            .collect();
        out.sort_by_key(|v| v.port);
        out
    }

    /// Protocol 2 allocations held.
    pub fn allocation_count(&self) -> usize {
        self.alloc.table.len()
    }

    /// Allocations `host` (its registered address) holds.
    pub fn clients_of(&self, host: SocketAddr) -> u32 {
        self.alloc.hosts.get(&host).map_or(0, |s| s.clients)
    }

    /// What the relay holds and what it refused.
    pub fn status(&self) -> RelayStatus {
        let a = &self.alloc;
        RelayStatus {
            protocol: RELAY_PROTOCOL_VERSION,
            lobbies: self.lobbies.len(),
            flows: self.flows.len() / 2,
            listings: self.directory.len(),
            allocations: a.table.len(),
            ports: a.known.len(),
            ports_free: a.free.len(),
            hosts: a.hosts.len(),
            most_clients_of_one_host: a.hosts.values().map(|s| s.clients).max().unwrap_or(0),
            max_clients_per_host: a.config.max_clients_per_host,
            forwarded: self.forwarded,
            forward_dropped: self.forward_dropped,
            rate_limited: self.rate_limited,
            ..a.status.clone()
        }
    }

    /// `Allocate` on the control port: the body after the head, `len` the whole datagram.
    pub(super) fn allocate(
        &mut self,
        from: SocketAddr,
        r: &mut WireReader<'_>,
        head: Frame,
        len: usize,
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        // `Allocate` exists only in the versioned framing: its minimum counts the whole datagram.
        if len < RELAY_ALLOCATE_REQUEST_BYTES || head.version < 2 {
            return Vec::new();
        }
        let Ok(host) = decode_addr(r) else {
            return Vec::new();
        };
        let (Ok(bind_key), Ok(client_key)) = (
            r.array::<RELAY_BIND_KEY_BYTES>(),
            r.array::<RELAY_ALLOCATION_KEY_BYTES>(),
        ) else {
            return Vec::new();
        };
        let cookie = read_cookie(r);
        let Some(lobby) = self.lobby_of(host, now_ms).filter(|_| host != from) else {
            return refusal(RelayError::NotAHost, from);
        };
        if !constant_time_eq(&lobby.bind_key, &bind_key) {
            return refusal(RelayError::BadKey, from);
        }
        let proved = cookie.is_some_and(|c| self.cookies.verify(from, COOKIE_ALLOCATE, &c, now_ms).is_some());
        let ask = |relay: &Self| challenge(relay.cookies.cookie(from, COOKIE_ALLOCATE, now_ms), from);
        // The same client key again: a lost answer (same address), or the client back from a
        // new address. An allocation in use is never moved by an `Allocate`.
        if let Some(&port) = self.alloc.by_client.get(&(host, client_key)) {
            let Some(a) = self.alloc.table.get(&port) else {
                return Vec::new();
            };
            if a.client != from {
                if now_ms.saturating_sub(a.client_sent_ms) < RELAY_BIND_REPLACE_IDLE_MS {
                    return refusal(RelayError::Busy, from);
                }
                if !proved {
                    return ask(self);
                }
                self.alloc.status.refreshed += 1;
            }
            let Some(a) = self.alloc.table.get_mut(&port) else {
                return Vec::new();
            };
            a.client = from;
            a.client_sent_ms = now_ms;
            let (host_addr, host_key) = (a.host, a.host_key);
            return self.allocation_answers(port, from, host, host_addr, host_key);
        }
        let config = &self.alloc.config;
        let block = AddrBlock::of(from.ip());
        let cap = if self.clients_of(host) >= config.max_clients_per_host {
            Some(config.max_clients_per_host)
        } else if self.alloc.per_ip.get(&block).copied().unwrap_or(0) >= config.max_allocations_per_ip {
            Some(config.max_allocations_per_ip)
        } else if self.alloc.free.is_empty() {
            Some(self.alloc.known.len() as u32)
        } else {
            None
        };
        if let Some(cap) = cap {
            self.alloc.status.full_refused += 1;
            return full(cap, from);
        }
        // Nothing is reserved for an address that has not proved it can read this.
        if !proved {
            return ask(self);
        }
        let Some(port) = self.alloc.free.pop_front() else {
            return full(0, from);
        };
        let host_key = self.alloc.host_key(port);
        let rate = self.alloc.config.allocation_bytes_per_second;
        let host_rate = self.alloc.config.host_bytes_per_second;
        self.alloc.table.insert(
            port,
            Allocation {
                lobby_host: host,
                host,
                client: from,
                client_key,
                host_key,
                block,
                to_host: Bucket::new(rate, now_ms),
                to_client: Bucket::new(rate, now_ms),
                client_sent_ms: now_ms,
                host_sent_ms: now_ms,
                bytes_to_host: 0,
                bytes_to_client: 0,
            },
        );
        self.alloc.by_client.insert((host, client_key), port);
        self.alloc
            .hosts
            .entry(host)
            .or_insert(HostShare {
                clients: 0,
                bytes: Bucket::new(host_rate, now_ms),
            })
            .clients += 1;
        *self.alloc.per_ip.entry(block).or_insert(0) += 1;
        self.allocation_answers(port, from, host, host, host_key)
    }

    /// `Allocated` to the client and `ClientJoined` to the host's registered address (both from
    /// the control port; the `Allocate` request is padded to more than both).
    fn allocation_answers(
        &self,
        port: u16,
        client: SocketAddr,
        lobby_host: SocketAddr,
        host: SocketAddr,
        host_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        let mut w = control_v(RelayTag::ClientJoined, RELAY_PROTOCOL_VERSION);
        w.u16(port);
        encode_addr(&mut w, client);
        w.raw(&host_key);
        vec![allocated(port, host, client), (w.finish(), lobby_host)]
    }

    /// Handles one datagram that arrived on allocation port `port` from `from`; returns what to
    /// send from that same port. Between the two permitted addresses the bytes are forwarded
    /// without parsing; the one exception is a protocol 2 `Refresh` (`RGNR | 0 | 2 | 15`), which
    /// no sealed game datagram begins with. Everything else is dropped and counted.
    pub fn handle_allocation(
        &mut self,
        port: u16,
        from: SocketAddr,
        datagram: &[u8],
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if now_ms >= self.next_expire_ms {
            self.expire(now_ms);
        }
        let Some(a) = self.alloc.table.get(&port) else {
            self.alloc.status.permission_dropped += 1;
            return Vec::new();
        };
        if let Some(head) = frame(datagram)
            && head.version >= 2
            && head.tag == RelayTag::Refresh as u8
        {
            return self.refresh(port, from, datagram, head, now_ms);
        }
        let side = if from == a.client {
            Side::Client
        } else if from == a.host {
            Side::Host
        } else {
            self.alloc.status.permission_dropped += 1;
            return Vec::new();
        };
        let bytes = datagram.len() as u64;
        let config = &self.alloc.config;
        let (rate, host_rate, relay_rate) = (
            config.allocation_bytes_per_second,
            config.host_bytes_per_second,
            config.relay_bytes_per_second,
        );
        let Some(a) = self.alloc.table.get_mut(&port) else {
            return Vec::new();
        };
        let (bucket, dst) = match side {
            Side::Client => {
                a.client_sent_ms = now_ms;
                (&mut a.to_host, a.host)
            }
            Side::Host => {
                a.host_sent_ms = now_ms;
                (&mut a.to_client, a.client)
            }
        };
        if !bucket.refill(rate, bytes, now_ms) {
            self.forward_dropped += 1;
            return Vec::new();
        }
        let lobby_host = a.lobby_host;
        let Some(share) = self.alloc.hosts.get_mut(&lobby_host) else {
            return Vec::new();
        };
        if !share.bytes.refill(host_rate, bytes, now_ms) {
            self.alloc.status.host_rate_dropped += 1;
            return Vec::new();
        }
        if !self.alloc.global.refill(relay_rate, bytes, now_ms) {
            self.alloc.status.relay_rate_dropped += 1;
            return Vec::new();
        }
        share.bytes.take(host_rate, bytes);
        self.alloc.global.take(relay_rate, bytes);
        if let Some(a) = self.alloc.table.get_mut(&port) {
            match side {
                Side::Client => {
                    a.to_host.take(rate, bytes);
                    a.bytes_to_host = a.bytes_to_host.saturating_add(bytes);
                }
                Side::Host => {
                    a.to_client.take(rate, bytes);
                    a.bytes_to_client = a.bytes_to_client.saturating_add(bytes);
                }
            }
        }
        self.forwarded += 1;
        vec![(datagram.to_vec(), dst)]
    }

    /// `Refresh` on allocation port `port`: confirms the sender's permission, or moves it to the
    /// sender's address once the key matched and the source proved itself with a cookie.
    fn refresh(
        &mut self,
        port: u16,
        from: SocketAddr,
        datagram: &[u8],
        head: Frame,
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if datagram.len() < RELAY_REFRESH_REQUEST_BYTES {
            return Vec::new();
        }
        if !self.sources.admit(from.ip(), now_ms) {
            self.rate_limited += 1;
            return Vec::new();
        }
        let mut r = WireReader::new(&datagram[head.body..]);
        let Ok(key) = r.array::<RELAY_ALLOCATION_KEY_BYTES>() else {
            return Vec::new();
        };
        let cookie = read_cookie(&mut r);
        let Some(a) = self.alloc.table.get(&port) else {
            return Vec::new();
        };
        // Both compared, so the time taken does not say which side a guess was near.
        let is_client = constant_time_eq(&a.client_key, &key);
        let is_host = constant_time_eq(&a.host_key, &key);
        let side = match (is_client, is_host) {
            (true, _) => Side::Client,
            (false, true) => Side::Host,
            (false, false) => {
                // A forger gets nothing back, not even a refusal.
                self.alloc.status.refresh_refused += 1;
                return Vec::new();
            }
        };
        let current = match side {
            Side::Client => a.client,
            Side::Host => a.host,
        };
        if from != current {
            let context = COOKIE_REFRESH | u64::from(port);
            let proved = cookie.is_some_and(|c| self.cookies.verify(from, context, &c, now_ms).is_some());
            if !proved {
                return challenge(self.cookies.cookie(from, context, now_ms), from);
            }
            self.alloc.status.refreshed += 1;
        }
        let Some(a) = self.alloc.table.get_mut(&port) else {
            return Vec::new();
        };
        let peer = match side {
            Side::Client => {
                a.client = from;
                a.client_sent_ms = now_ms;
                a.host
            }
            Side::Host => {
                a.host = from;
                a.host_sent_ms = now_ms;
                a.client
            }
        };
        vec![allocated(port, peer, from)]
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    const BASE: u16 = 50_000;

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([203, 0, 113, last], port))
    }

    fn relay(config: RelayConfig, ports: u16) -> RelayCore {
        let mut relay = RelayCore::with_config(1, [7; 32], config);
        relay.add_allocation_ports(BASE..BASE + ports);
        relay
    }

    fn decode(out: &[(Vec<u8>, SocketAddr)]) -> Vec<(RelayReply, SocketAddr)> {
        out.iter()
            .map(|(b, to)| (RelayReply::decode(b).expect("valid").expect("relay traffic"), *to))
            .collect()
    }

    fn only(out: &[(Vec<u8>, SocketAddr)]) -> RelayReply {
        let mut replies = decode(out);
        assert_eq!(replies.len(), 1, "one reply: {replies:?}");
        replies.remove(0).0
    }

    /// Registers `host` (challenge and cookie) and returns its bind key.
    fn register(relay: &mut RelayCore, host: SocketAddr, now: u64) -> [u8; RELAY_BIND_KEY_BYTES] {
        let first = relay.handle(host, &encode_register(3600, b"").expect("fits"), now);
        let RelayReply::RegisterChallenge(cookie) = only(&first) else {
            panic!("expected a challenge")
        };
        let again = relay.handle(
            host,
            &encode_register_with_cookie(3600, b"", &cookie).expect("fits"),
            now,
        );
        let RelayReply::JoinCode(code) = only(&again) else {
            panic!("expected a code")
        };
        relay.bind_key(&code, host)
    }

    /// `Allocate` with its cookie round trip; the replies of the second request.
    fn allocate(
        relay: &mut RelayCore,
        client: SocketAddr,
        host: SocketAddr,
        bind_key: &[u8; RELAY_BIND_KEY_BYTES],
        client_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
        now: u64,
    ) -> Vec<(RelayReply, SocketAddr)> {
        let first = relay.handle(client, &encode_allocate(host, bind_key, &client_key, None), now);
        match decode(&first).as_slice() {
            [(RelayReply::Challenge(cookie), to)] => {
                assert_eq!(*to, client);
                let again = encode_allocate(host, bind_key, &client_key, Some(cookie));
                decode(&relay.handle(client, &again, now))
            }
            _ => decode(&first),
        }
    }

    /// The port of an allocation that succeeded; checks the host was told.
    fn port_of(replies: &[(RelayReply, SocketAddr)], client: SocketAddr, host: SocketAddr) -> u16 {
        let [
            (RelayReply::Allocated { port, peer }, to),
            (
                RelayReply::ClientJoined {
                    port: p2, client: c, ..
                },
                h,
            ),
        ] = replies
        else {
            panic!("expected Allocated and ClientJoined, got {replies:?}");
        };
        assert_eq!((*peer, *to, *p2, *c, *h), (host, client, *port, client, host));
        *port
    }

    fn host_key_of(replies: &[(RelayReply, SocketAddr)]) -> [u8; RELAY_ALLOCATION_KEY_BYTES] {
        replies
            .iter()
            .find_map(|(r, _)| match r {
                RelayReply::ClientJoined { host_key, .. } => Some(*host_key),
                _ => None,
            })
            .expect("a ClientJoined")
    }

    /// Acceptance (N112): the second client of a host is accepted while the first plays. On the
    /// base the only way in was a protocol 1 `Bind`, and it answered `Busy` (still true for
    /// protocol 1, which keeps its single-client semantics).
    #[test]
    fn a_second_client_is_accepted_while_the_first_plays() {
        let mut relay = relay(RelayConfig::default(), 16);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let (a, b) = (addr(2, 5000), addr(3, 5000));
        // Protocol 1: one client per host address.
        assert!(relay.handle(a, &encode_bind(host, &key), 0).is_empty());
        assert_eq!(relay.handle(a, b"play", 10), vec![(b"play".to_vec(), host)]);
        assert_eq!(
            only(&relay.handle(b, &encode_bind(host, &key), 20)),
            RelayReply::Error(RelayError::Busy),
            "protocol 1 keeps one relayed client per host"
        );
        // Protocol 2: each client its own allocation.
        let (c, d) = (addr(4, 5000), addr(5, 5000));
        let pc = port_of(&allocate(&mut relay, c, host, &key, [1; 16], 30), c, host);
        assert_eq!(
            relay.handle_allocation(pc, c, b"c plays", 40),
            vec![(b"c plays".to_vec(), host)]
        );
        let pd = port_of(&allocate(&mut relay, d, host, &key, [2; 16], 50), d, host);
        assert_ne!(pc, pd);
        assert_eq!(
            relay.handle_allocation(pd, d, b"d plays", 60),
            vec![(b"d plays".to_vec(), host)]
        );
        // The host answers each from that client's port, and each reaches only its own client.
        assert_eq!(
            relay.handle_allocation(pc, host, b"to c", 70),
            vec![(b"to c".to_vec(), c)]
        );
        assert_eq!(
            relay.handle_allocation(pd, host, b"to d", 70),
            vec![(b"to d".to_vec(), d)]
        );
        assert_eq!(relay.clients_of(host), 2);
        assert_eq!(relay.flow_count(), 1, "the protocol 1 flow is untouched");
        assert_eq!(relay.handle(a, b"still", 80), vec![(b"still".to_vec(), host)]);
    }

    /// Acceptance (N112): 32 clients each get their own port; the 33rd over a cap of 32 gets
    /// `Full { cap: 32 }`; the per-IP cap and the pool answer `Full` with their own caps.
    #[test]
    fn caps_answer_full_with_their_value() {
        let config = RelayConfig {
            max_clients_per_host: 32,
            ..RelayConfig::default()
        };
        let mut relay = relay(config, 64);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let mut ports = HashSet::new();
        for i in 0..32u8 {
            let client = addr(10 + i, 6000);
            let port = port_of(
                &allocate(&mut relay, client, host, &key, [i; 16], 1),
                client,
                host,
            );
            assert!(ports.insert(port));
            assert_eq!(
                relay.handle_allocation(port, client, &[i], 2),
                vec![(vec![i], host)]
            );
        }
        let late = addr(99, 6000);
        let first = relay.handle(late, &encode_allocate(host, &key, &[99; 16], None), 3);
        assert_eq!(
            only(&first),
            RelayReply::Full { cap: 32 },
            "the cap is answered before any cookie"
        );
        assert_eq!(relay.allocation_count(), 32);
        assert_eq!(relay.status().full_refused, 1);

        // Per source IP: the ninth allocation of one address block.
        let other_host = addr(2, 7777);
        let other_key = register(&mut relay, other_host, 4);
        for i in 0..RELAY_MAX_ALLOCATIONS_PER_IP as u16 {
            let client = addr(200, 7000 + i);
            port_of(
                &allocate(&mut relay, client, other_host, &other_key, [100 + i as u8; 16], 5),
                client,
                other_host,
            );
        }
        let ninth = addr(200, 7999);
        assert_eq!(
            only(&relay.handle(
                ninth,
                &encode_allocate(other_host, &other_key, &[250; 16], None),
                6
            )),
            RelayReply::Full {
                cap: RELAY_MAX_ALLOCATIONS_PER_IP as u16
            }
        );

        // The pool: a relay with two ports.
        let mut small = self::relay(RelayConfig::default(), 2);
        let key = register(&mut small, host, 0);
        for i in 0..2u8 {
            port_of(
                &allocate(&mut small, addr(30 + i, 1), host, &key, [i; 16], 1),
                addr(30 + i, 1),
                host,
            );
        }
        assert_eq!(
            only(&small.handle(addr(40, 1), &encode_allocate(host, &key, &[9; 16], None), 2)),
            RelayReply::Full { cap: 2 }
        );
        // No ports at all (a protocol 1 relay build): `Full { cap: 0 }`, never silence.
        let mut none = self::relay(RelayConfig::default(), 0);
        let key = register(&mut none, host, 0);
        assert_eq!(
            only(&none.handle(addr(40, 1), &encode_allocate(host, &key, &[9; 16], None), 2)),
            RelayReply::Full { cap: 0 }
        );
    }

    /// Acceptance (N112), audit H2: a forger on a third address that knows the port can neither
    /// spend the allocation's budget nor block it; it cannot move a permission without the key.
    #[test]
    fn a_forger_cannot_spend_or_block_an_allocation() {
        let config = RelayConfig {
            allocation_bytes_per_second: 10_000,
            ..RelayConfig::default()
        };
        let mut relay = relay(config, 4);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let port = port_of(
            &allocate(&mut relay, client, host, &key, [1; 16], 0),
            client,
            host,
        );
        let forger = addr(66, 6666);
        for i in 0..1000 {
            assert!(
                relay
                    .handle_allocation(port, forger, &[0xEE; 1000], 1 + i % 3)
                    .is_empty()
            );
        }
        assert_eq!(relay.status().permission_dropped, 1000);
        // The client still has its whole budget this second.
        for _ in 0..10 {
            assert_eq!(relay.handle_allocation(port, client, &[1; 1000], 5).len(), 1);
        }
        assert!(
            relay.handle_allocation(port, client, &[1; 1000], 5).is_empty(),
            "the budget is the client's own"
        );
        // Refreshes with guessed keys get nothing back, not even a challenge.
        for guess in (0..40u8).filter(|g| *g != 1) {
            let refresh = encode_refresh(&[guess; 16], None);
            assert!(relay.handle_allocation(port, forger, &refresh, 6).is_empty());
        }
        assert_eq!(
            relay.status().refresh_refused,
            39,
            "every guess that is not the client's key ([1; 16])"
        );
        // [1; 16] is the client key: from the forger's address it only gets a cookie challenge,
        // which a forged source cannot read; the permission is still the client's.
        let refresh = encode_refresh(&[1; 16], None);
        assert!(matches!(
            only(&relay.handle_allocation(port, forger, &refresh, 7)),
            RelayReply::Challenge(_)
        ));
        assert_eq!(
            relay.handle_allocation(port, host, b"to client", 1_100),
            vec![(b"to client".to_vec(), client)]
        );
        // An `Allocate` with the client's key from another address cannot move it while in use.
        let replies = relay.handle(forger, &encode_allocate(host, &key, &[1; 16], None), 1_200);
        assert_eq!(only(&replies), RelayReply::Error(RelayError::Busy));
        // A wrong bind key is refused.
        let replies = relay.handle(forger, &encode_allocate(host, &[0; 16], &[7; 16], None), 1_300);
        assert_eq!(only(&replies), RelayReply::Error(RelayError::BadKey));
    }

    /// RFC 8656 § 9: a client whose NAT rebound refreshes its permission with its key and a
    /// cookie; its old address is no longer permitted. A host whose game socket differs from its
    /// registered one moves the host side the same way.
    #[test]
    fn a_rebinding_side_refreshes_its_permission() {
        let mut relay = relay(RelayConfig::default(), 4);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let replies = allocate(&mut relay, client, host, &key, [1; 16], 0);
        let port = port_of(&replies, client, host);
        let host_key = host_key_of(&replies);
        // The client's NAT moved it to a new port.
        let moved = addr(2, 5001);
        assert!(relay.handle_allocation(port, moved, b"lost", 10).is_empty());
        let refresh = encode_refresh(&[1; 16], None);
        let RelayReply::Challenge(cookie) = only(&relay.handle_allocation(port, moved, &refresh, 20)) else {
            panic!("expected a challenge")
        };
        let refresh = encode_refresh(&[1; 16], Some(&cookie));
        assert_eq!(
            only(&relay.handle_allocation(port, moved, &refresh, 30)),
            RelayReply::Allocated { port, peer: host }
        );
        assert_eq!(
            relay.handle_allocation(port, moved, b"back", 40),
            vec![(b"back".to_vec(), host)]
        );
        assert!(
            relay.handle_allocation(port, client, b"old", 50).is_empty(),
            "the old address lost its permission"
        );
        assert_eq!(
            relay.handle_allocation(port, host, b"hi", 60),
            vec![(b"hi".to_vec(), moved)]
        );
        // The host's game socket is another one than the registered.
        let game = addr(1, 7778);
        assert!(relay.handle_allocation(port, game, b"x", 70).is_empty());
        let refresh = encode_refresh(&host_key, None);
        let RelayReply::Challenge(cookie) = only(&relay.handle_allocation(port, game, &refresh, 80)) else {
            panic!("expected a challenge")
        };
        let refresh = encode_refresh(&host_key, Some(&cookie));
        assert_eq!(
            only(&relay.handle_allocation(port, game, &refresh, 90)),
            RelayReply::Allocated { port, peer: moved }
        );
        assert_eq!(
            relay.handle_allocation(port, game, b"state", 100),
            vec![(b"state".to_vec(), moved)]
        );
        assert_eq!(
            relay.handle_allocation(port, moved, b"input", 110),
            vec![(b"input".to_vec(), game)]
        );
        // A refresh from the address already permitted confirms it (and opens the sender's NAT).
        let refresh = encode_refresh(&host_key, None);
        assert_eq!(
            only(&relay.handle_allocation(port, game, &refresh, 120)),
            RelayReply::Allocated { port, peer: moved }
        );
        assert_eq!(relay.status().refreshed, 2);
    }

    /// An idle allocation (5 s) comes back to the same client key from a new address, on the same
    /// port; an active one is never displaced; another key never takes it.
    #[test]
    fn the_same_client_key_returns_to_an_idle_allocation_only() {
        let mut relay = relay(RelayConfig::default(), 4);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let port = port_of(
            &allocate(&mut relay, client, host, &key, [1; 16], 0),
            client,
            host,
        );
        relay.handle_allocation(port, client, b"x", 1_000);
        let new_addr = addr(3, 5000);
        assert_eq!(
            only(&relay.handle(new_addr, &encode_allocate(host, &key, &[1; 16], None), 2_000)),
            RelayReply::Error(RelayError::Busy)
        );
        // The host keeps sending; only the client's own traffic counts.
        relay.handle_allocation(port, host, b"y", 5_500);
        let replies = allocate(&mut relay, new_addr, host, &key, [1; 16], 6_100);
        assert_eq!(
            port_of(&replies, new_addr, host),
            port,
            "the same port comes back"
        );
        assert_eq!(relay.allocation_count(), 1);
        assert_eq!(
            relay.handle_allocation(port, new_addr, b"z", 6_200),
            vec![(b"z".to_vec(), host)]
        );
        // A lost answer: the same request from the same address is answered again.
        let again = relay.handle(new_addr, &encode_allocate(host, &key, &[1; 16], None), 6_300);
        assert_eq!(decode(&again)[0].0, RelayReply::Allocated { port, peer: host });
    }

    /// Nothing is reserved for an address that has not proved it can read the relay's answers.
    #[test]
    fn an_allocate_must_prove_its_source() {
        let mut relay = relay(RelayConfig::default(), 4);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        for i in 0..50u8 {
            let forged = addr(100 + i, 1);
            let out = relay.handle(forged, &encode_allocate(host, &key, &[i; 16], None), 1);
            assert!(matches!(only(&out), RelayReply::Challenge(_)));
            let wrong = encode_allocate(host, &key, &[i; 16], Some(&[0; RELAY_COOKIE_BYTES]));
            assert!(matches!(
                only(&relay.handle(forged, &wrong, 1)),
                RelayReply::Challenge(_)
            ));
        }
        assert_eq!(relay.allocation_count(), 0);
        assert_eq!(relay.status().ports_free, 4);
    }

    /// Anti-amplification (ADR-1892) kept: no allocation request is answered with more bytes than
    /// it carried, IPv6 addresses included; short requests get nothing.
    #[test]
    fn allocation_requests_are_never_amplifiers() {
        let mut relay = relay(RelayConfig::default(), 4);
        let host = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 7777));
        let key = register(&mut relay, host, 0);
        let client = SocketAddr::from(([0x2001, 0xdb8, 1, 0, 0, 0, 0, 9], 40000));
        let request = encode_allocate(host, &key, &[1; 16], None);
        let first = relay.handle(client, &request, 1);
        let RelayReply::Challenge(cookie) = only(&first) else {
            panic!("expected a challenge")
        };
        let request = encode_allocate(host, &key, &[1; 16], Some(&cookie));
        let out = relay.handle(client, &request, 1);
        let sent: usize = out.iter().map(|(b, _)| b.len()).sum();
        assert_eq!(out.len(), 2);
        assert!(
            sent <= request.len(),
            "{sent} bytes for a {}-byte Allocate",
            request.len()
        );
        let port = BASE;
        for request in [
            encode_refresh(&[1; 16], None),
            encode_refresh(&host_key_of(&decode(&out)), None),
        ] {
            let moved = SocketAddr::from(([0x2001, 0xdb8, 2, 0, 0, 0, 0, 9], 40001));
            for from in [client, moved] {
                let out = relay.handle_allocation(port, from, &request, 2);
                let sent: usize = out.iter().map(|(b, _)| b.len()).sum();
                assert!(
                    sent <= request.len(),
                    "{sent} bytes for a {}-byte Refresh",
                    request.len()
                );
            }
        }
        assert_eq!(request.len(), RELAY_ALLOCATE_REQUEST_BYTES);
        // Too short by one byte: dropped.
        let mut short = encode_allocate(host, &key, &[2; 16], None);
        short.truncate(RELAY_ALLOCATE_REQUEST_BYTES - 1);
        assert!(relay.handle(addr(9, 9), &short, 3).is_empty());
        let mut short = encode_refresh(&[1; 16], None);
        short.truncate(RELAY_REFRESH_REQUEST_BYTES - 1);
        assert!(relay.handle_allocation(port, addr(9, 9), &short, 3).is_empty());
        // Versioned refusals are no larger than their request either.
        let mut old = encode_allocate(host, &key, &[2; 16], None);
        old.drain(4..6);
        let out = relay.handle(addr(9, 10), &old[..6], 4);
        assert_eq!(only(&out), RelayReply::VersionTooOld { min: 2 });
        assert!(out[0].0.len() <= 6);
        assert!(
            relay.handle(addr(9, 11), &old[..5], 4).is_empty(),
            "five bytes cannot carry the answer"
        );
    }

    /// Protocol 1 requests keep their protocol 1 answers; their versioned form gets the same
    /// answer in the versioned framing; allocation tags in protocol 1 framing get
    /// `VersionTooOld`, newer protocols `VersionTooNew`.
    #[test]
    fn both_framings_are_answered_in_their_own() {
        let mut relay = relay(RelayConfig::default(), 4);
        let host = addr(1, 7777);
        register(&mut relay, host, 0);
        let lookup = encode_lookup("AAAAAA").expect("code");
        let v1 = relay.handle(addr(2, 1), &lookup, 1);
        assert_eq!(relay_version(&v1[0].0), Some(1));
        let v2 = relay.handle(addr(2, 1), &versioned(&lookup), 1);
        assert_eq!(relay_version(&v2[0].0), Some(2));
        assert_eq!(only(&v1), only(&v2));
        assert_eq!(only(&v1), RelayReply::Error(RelayError::UnknownCode));
        // A versioned register works too, and its challenge comes back versioned.
        let reg = relay.handle(
            addr(3, 1),
            &versioned(&encode_register(60, b"").expect("fits")),
            2,
        );
        assert_eq!(relay_version(&reg[0].0), Some(2));
        assert!(matches!(only(&reg), RelayReply::RegisterChallenge(_)));
        for tag in RelayTag::Allocate as u8..=RelayTag::VersionTooNew as u8 {
            let mut old = control(RelayTag::Lookup).finish();
            old[4] = tag;
            old.resize(RELAY_ALLOCATE_REQUEST_BYTES, 0);
            assert_eq!(
                only(&relay.handle(addr(4, 1), &old, 3)),
                RelayReply::VersionTooOld { min: 2 },
                "tag {tag}"
            );
        }
        let mut newer = versioned(&lookup);
        newer[5] = 9;
        assert_eq!(
            only(&relay.handle(addr(5, 1), &newer, 4)),
            RelayReply::VersionTooNew {
                max: RELAY_PROTOCOL_VERSION
            }
        );
        // A versioned head that claims protocol 0 or 1 is malformed: dropped.
        for bad in [0u8, 1] {
            let mut odd = versioned(&lookup);
            odd[5] = bad;
            assert!(relay.handle(addr(6, 1), &odd, 5).is_empty());
        }
        assert!(relay.status().version_refused >= 9);
    }

    /// Per host, the allocations together forward at most `host_bytes_per_second`.
    #[test]
    fn a_host_share_is_capped() {
        let config = RelayConfig {
            host_bytes_per_second: 3_000,
            ..RelayConfig::default()
        };
        let mut relay = relay(config, 8);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let ports: Vec<(u16, SocketAddr)> = (0..4u8)
            .map(|i| {
                let c = addr(10 + i, 1);
                (
                    port_of(&allocate(&mut relay, c, host, &key, [i; 16], 0), c, host),
                    c,
                )
            })
            .collect();
        let forwarded = ports
            .iter()
            .filter(|(port, c)| !relay.handle_allocation(*port, *c, &[0; 1000], 1).is_empty())
            .count();
        assert_eq!(forwarded, 3, "4 x 1000 bytes against a 3000-byte host share");
        assert_eq!(relay.status().host_rate_dropped, 1);
        // Another host is not affected.
        let other = addr(2, 7777);
        let key = register(&mut relay, other, 0);
        let c = addr(50, 1);
        let port = port_of(&allocate(&mut relay, c, other, &key, [9; 16], 1), c, other);
        assert_eq!(relay.handle_allocation(port, c, &[0; 1000], 1).len(), 1);
        // The share refills with time.
        assert_eq!(
            relay
                .handle_allocation(ports[3].0, ports[3].1, &[0; 1000], 400)
                .len(),
            1
        );
    }

    /// Idle allocations expire; their ports go to the back of the pool and every count drops.
    #[test]
    fn idle_allocations_expire_and_free_their_ports_last_reused() {
        let mut relay = relay(RelayConfig::default(), 3);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let c = addr(2, 1);
        let first = port_of(&allocate(&mut relay, c, host, &key, [1; 16], 0), c, host);
        assert_eq!(first, BASE);
        relay.expire(RELAY_FLOW_IDLE_MS);
        let status = relay.status();
        assert_eq!((status.allocations, status.ports_free, status.hosts), (0, 3, 0));
        assert_eq!(relay.clients_of(host), 0);
        assert!(
            relay
                .handle_allocation(first, c, b"late", RELAY_FLOW_IDLE_MS + 1)
                .is_empty()
        );
        let next = port_of(
            &allocate(&mut relay, c, host, &key, [1; 16], RELAY_FLOW_IDLE_MS + 2),
            c,
            host,
        );
        assert_ne!(next, first, "a released port is reused last");
    }

    #[test]
    fn hostile_allocation_bytes_never_panic() {
        let mut relay = relay(RelayConfig::default(), 2);
        let host = addr(1, 7777);
        let key = register(&mut relay, host, 0);
        let c = addr(2, 1);
        let port = port_of(&allocate(&mut relay, c, host, &key, [1; 16], 0), c, host);
        let mut seed = 0x1234_5678u32;
        for i in 0..4000u64 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let len = (seed % 200) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|j| (seed >> (j % 24)) as u8).collect();
            if bytes.len() >= 7 && i % 2 == 0 {
                bytes[..4].copy_from_slice(&RELAY_MAGIC);
                bytes[4] = RELAY_VERSIONED;
                bytes[5] = 2;
                bytes[6] = 12 + (seed % 8) as u8;
            }
            let from = if i % 3 == 0 { c } else { addr((seed % 7) as u8, 9) };
            let _ = relay.handle(from, &bytes, 10 + i);
            let _ = relay.handle_allocation(port, from, &bytes, 10 + i);
            let _ = relay.handle_allocation(port + 1, from, &bytes, 10 + i);
            for (out, _) in relay.handle(from, &bytes, 10 + i) {
                let _ = RelayReply::decode(&out);
            }
            let _ = RelayReply::decode(&bytes);
        }
    }

    #[test]
    fn relay_config_defaults_follow_spec_120() {
        let c = RelayConfig::default();
        assert_eq!(
            (
                c.allocation_ports,
                c.max_clients_per_host,
                c.max_allocations_per_ip
            ),
            (4096, 40, 8)
        );
        assert_eq!(c.allocation_bytes_per_second, 4 * 1024 * 1024);
        assert_eq!(c.host_bytes_per_second, 64 * 1024 * 1024);
        assert_eq!(c.relay_bytes_per_second, 0);
    }
}
