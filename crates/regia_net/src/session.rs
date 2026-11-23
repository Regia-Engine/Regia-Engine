// SPDX-License-Identifier: MPL-2.0
//! Session registry: peers, health and bans.
//!
//! The server owns one [`Session`]. Peer ids are small integers, never 0, and not reused after a
//! peer leaves until every id up to `u16::MAX` has been issued; a live id is never reissued
//! (ADR-5580). Health is a summary of the link (round-trip time, jitter,
//! loss, silence); loss and latency only *flag* a peer, they never remove it. Silence of at least
//! the timeout is the only automatic removal; kicks and bans are explicit host decisions.
//!
//! The session lifecycle of spec 77 lives in the child modules: the state machines
//! ([`lifecycle`]), the wire messages ([`proto`]), the content manifest ([`manifest`]), the ban
//! list ([`bans`]), streams ([`streams`]), the conditioner ([`conditioner`]), loopback and simulated
//! clients ([`hub`]) and the ECS resource that drives them (`net_session`, `NetSession`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};

#[cfg(feature = "ecs")]
use regia_ecs::prelude::{Reflect, bevy_reflect};
use serde::{Deserialize, Serialize};

use crate::addr_block::same_block;
use crate::connection::LinkStats;
use crate::handshake::PeerRole;

pub mod bans;
pub mod conditioner;
pub mod hub;
pub mod lifecycle;
pub mod manifest;
pub mod meter;
#[cfg(feature = "ecs")]
pub mod net_session;
#[cfg(feature = "ecs")]
pub mod players;
pub mod proto;
pub mod streams;
#[cfg(feature = "ecs")]
pub mod travel;

/// Bans one endpoint keeps; the oldest goes first.
pub const MAX_BANS: usize = 4096;
/// Longest ban reason, bytes.
pub const MAX_BAN_REASON_BYTES: usize = 256;

pub use bans::{BanId, BanKind, BanList, BanListEntry, BanStore};
#[cfg(debug_assertions)]
pub use conditioner::LinkShaper;
pub use conditioner::{SessionConditioner, ShaperSettings};
pub use hub::{LoopbackClients, SessionHub, SimInput};
pub use lifecycle::{
    AuthRequestId, AuthResult, ClientSessionEvent, ClientSessionState, HostOptions, JoinDecision,
    JoinSnapshot, LinkId, PeerState, ServerSessionEvent, SessionAuthenticator, SessionClient, SessionServer,
    SessionSettings,
};
pub use manifest::{ContentManifest, ManifestAsset, ManifestDiffItem, ManifestMod};
#[cfg(feature = "ecs")]
pub use net_session::{
    ConnectOptions, ConnectTarget, MAX_DEFERRED_RELIABLE_BYTES, NET_MANIFEST_ENV, NetManifestFile,
    NetSession, SessionError, SessionRules, SocketOpener, udp_socket_opener,
};
#[cfg(feature = "ecs")]
pub use players::{
    HandOffRecord, NetDespawnWithOwner, NetHandOffs, NetPlayerHooks, NetPlayerStats, NetStatsOverlay,
    NetSuccessor, OwnedHandOff, PlayerHook, PlayerLinkStats,
};
pub use proto::TravelKeep;
pub use proto::{DisconnectReason, Hello, SessionMsg, Welcome};
pub use streams::{StreamEvent, StreamId};
#[cfg(feature = "ecs")]
pub use travel::{
    JoinRecord, JoinRecordKind, JoinSnapshotCounts, NetPersistAcrossTravel, TravelCounts, despawn_for_travel,
    join_records_for, join_snapshot_for, join_snapshots_for,
};

/// A peer's id in a session: one end of a connection as the server sees it. `PeerId::SERVER` (0)
/// is the server process itself (also the host of a listen server) and is never assigned to a
/// client.
///
/// [`crate::session::SessionServer`] (the allocator `crate::session::NetSession` drives) issues
/// client ids from 1 upward in handshake order and never reuses one in that session. `u32::MAX`
/// is not issued; a join that would need it is refused with `DisconnectReason::ServerFull`
/// (spec 70 § 4). The transport [`Session`] issues link ids in `1..=u16::MAX`; once all have been
/// issued it starts again at 1 and skips live ids (ADR-1643, ADR-5580).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Reflect))]
pub struct PeerId(pub u32);

impl PeerId {
    /// The server process itself.
    pub const SERVER: PeerId = PeerId(0);
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer{}", self.0)
    }
}

/// Silence policy shared by session and datagram endpoints (spec 120 § 10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeoutPolicy {
    /// Silence before one interruption notification, milliseconds.
    pub interrupt_ms: u64,
    /// Silence before removal after startup, milliseconds.
    pub timeout_ms: u64,
    /// Minimum time from admission before removing a peer that has sent no game data.
    pub start_grace_ms: u64,
}
impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            interrupt_ms: 1_500,
            timeout_ms: 20_000,
            start_grace_ms: 30_000,
        }
    }
}
impl TimeoutPolicy {
    /// Shortest `timeout_ms` [`Self::sanitized`] keeps (1 s): a value written in seconds by
    /// mistake (`timeout_ms = 20`) would drop every peer at once.
    pub const MIN_TIMEOUT_MS: u64 = 1_000;
    /// Shortest `interrupt_ms` [`Self::sanitized`] keeps: `0` would raise an interruption and a
    /// restoration every tick.
    pub const MIN_INTERRUPT_MS: u64 = 100;
    /// Longest `start_grace_ms` [`Self::sanitized`] keeps (10 minutes).
    pub const MAX_START_GRACE_MS: u64 = 600_000;

    /// The policy with every value in range: `timeout_ms` at least [`Self::MIN_TIMEOUT_MS`],
    /// `interrupt_ms` from [`Self::MIN_INTERRUPT_MS`] up to `timeout_ms - 1`, `start_grace_ms` at
    /// most [`Self::MAX_START_GRACE_MS`]. Endpoints resolve their policy through it; a dedicated
    /// server's `server.toml` refuses out-of-range values instead.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let timeout_ms = self.timeout_ms.max(Self::MIN_TIMEOUT_MS);
        Self {
            timeout_ms,
            interrupt_ms: self.interrupt_ms.clamp(Self::MIN_INTERRUPT_MS, timeout_ms - 1),
            start_grace_ms: self.start_grace_ms.min(Self::MAX_START_GRACE_MS),
        }
    }

    /// Whether silence has exhausted the policy. Startup grace is an absolute floor from
    /// admission, not an extra interval added to every subsequent outage.
    pub fn expired(self, now_ms: u64, joined_ms: u64, last_seen_ms: u64, started: bool) -> bool {
        now_ms.saturating_sub(last_seen_ms) >= self.timeout_ms
            && (started || now_ms.saturating_sub(joined_ms) >= self.start_grace_ms)
    }
}

/// Thresholds that flag a peer in diagnostics (never a kick by themselves).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthPolicy {
    /// Loss fraction above which a peer is flagged.
    pub flag_loss_above: f32,
    /// Round-trip time above which a peer is flagged, in milliseconds.
    pub flag_rtt_above_ms: f64,
    /// Silence after which a peer is removed, in milliseconds.
    pub timeout_ms: u64,
    /// Silence before interruption, milliseconds.
    pub interrupt_ms: u64,
    /// Startup grace before the first game datagram, milliseconds.
    pub start_grace_ms: u64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        let timeout = TimeoutPolicy::default();
        Self {
            flag_loss_above: 0.05,
            flag_rtt_above_ms: 250.0,
            timeout_ms: timeout.timeout_ms,
            interrupt_ms: timeout.interrupt_ms,
            start_grace_ms: timeout.start_grace_ms,
        }
    }
}

impl HealthPolicy {
    /// Replaces only the silence policy, preserving diagnostic thresholds.
    pub fn with_timeouts(mut self, policy: TimeoutPolicy) -> Self {
        self.timeout_ms = policy.timeout_ms;
        self.interrupt_ms = policy.interrupt_ms;
        self.start_grace_ms = policy.start_grace_ms;
        self
    }
    /// The endpoint's resolved silence policy.
    pub fn timeouts(self) -> TimeoutPolicy {
        TimeoutPolicy {
            timeout_ms: self.timeout_ms,
            interrupt_ms: self.interrupt_ms,
            start_grace_ms: self.start_grace_ms,
        }
    }
}

/// A health summary for one peer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PeerHealth {
    /// Smoothed round-trip time, milliseconds.
    pub rtt_ms: f64,
    /// Round-trip variation, milliseconds.
    pub jitter_ms: f64,
    /// Recent loss fraction.
    pub loss_fraction: f32,
    /// Milliseconds since the last packet from this peer.
    pub silent_ms: u64,
    /// True when loss or latency is above the policy thresholds.
    pub flagged: bool,
}

/// One connected peer.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Session id.
    pub id: PeerId,
    /// Source address. Change it with [`Session::set_peer_addr`], which keeps the address index.
    pub addr: SocketAddr,
    /// Display name from the join token.
    pub name: String,
    /// Player or spectator.
    pub role: PeerRole,
    /// When it joined (endpoint clock, milliseconds).
    pub joined_ms: u64,
    /// Last packet from it (endpoint clock, milliseconds).
    pub last_seen_ms: u64,
    /// Latest link statistics.
    pub link: LinkStats,
    /// Ticks the tick driver advanced without this peer's input.
    pub missed_input_ticks: u64,
    /// Whether an authentic game datagram has arrived since admission.
    pub started: bool,
    /// Whether the peer is currently interrupted.
    pub interrupted: bool,
}

impl Peer {
    fn new(id: PeerId, addr: SocketAddr, name: String, role: PeerRole, now_ms: u64) -> Self {
        Self {
            id,
            addr,
            name,
            role,
            joined_ms: now_ms,
            last_seen_ms: now_ms,
            link: LinkStats::default(),
            missed_input_ticks: 0,
            started: false,
            interrupted: false,
        }
    }
}

/// A ban on an IP address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanEntry {
    /// The banned address (any port).
    pub ip: IpAddr,
    /// Why, for the host's records.
    pub reason: String,
    /// Unix time in seconds when the ban ends; `0` is permanent.
    pub expires_unix_s: u64,
    /// `true` bans exactly [`Self::ip`]; `false` (the default) bans its block: the IPv4 address,
    /// or the whole IPv6 /64 ([`crate::addr_block`], ADR-3283). Mirrors an explicit `/128` in a
    /// `bans.ron` address entry so both ports enforce the same reach (ADR-4936).
    #[serde(default)]
    pub exact: bool,
}

/// Peers and bans of one session.
#[derive(Debug, Clone)]
pub struct Session {
    max_peers: usize,
    policy: HealthPolicy,
    peers: BTreeMap<PeerId, Peer>,
    /// Address -> peer, so a datagram finds its peer in one probe whatever the peer count
    /// (ADR-8299). Kept by [`Self::add_peer`], [`Self::remove_peer`] and [`Self::set_peer_addr`].
    by_addr: HashMap<SocketAddr, PeerId>,
    /// Map probes [`Self::peer_by_addr`] made (one per lookup), for the flat-cost test.
    addr_probes: u64,
    bans: Vec<BanEntry>,
    next_id: u16,
    /// Ids of dormant peers (spec 120 § 10.3): no connection, but the slot and the id are held for
    /// a resume, so [`Self::add_peer`] skips them and they count toward the capacity.
    reserved: BTreeSet<PeerId>,
}

impl Session {
    /// An empty session for up to `max_peers` peers.
    pub fn new(max_peers: usize, policy: HealthPolicy) -> Self {
        Self {
            max_peers: max_peers.min(usize::from(u16::MAX) - 1),
            policy,
            peers: BTreeMap::new(),
            by_addr: HashMap::new(),
            addr_probes: 0,
            bans: Vec::new(),
            next_id: 1,
            reserved: BTreeSet::new(),
        }
    }

    /// The health policy.
    pub fn policy(&self) -> &HealthPolicy {
        &self.policy
    }

    /// Peer capacity.
    pub fn max_peers(&self) -> usize {
        self.max_peers
    }

    /// True when no slot is free.
    pub fn is_full(&self) -> bool {
        self.peers.len() + self.reserved.len() >= self.max_peers
    }

    /// Holds `id` for a resume (a dormant peer, spec 120 § 10.3): the id is not issued again and
    /// the slot stays taken until [`Self::release`] or [`Self::add_peer_as`].
    pub fn reserve(&mut self, id: PeerId) {
        self.reserved.insert(id);
    }

    /// Frees a held id (the dormant peer's grace ended). True when it was held.
    pub fn release(&mut self, id: PeerId) -> bool {
        self.reserved.remove(&id)
    }

    /// Ids held for a resume, in id order.
    pub fn reserved(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.reserved.iter().copied()
    }

    /// Adds a peer under a held id (a resume, spec 120 § 10.3). Its own held slot makes room, so a
    /// returning peer is never refused `full` by its own dormant slot. `false` when `id` is live.
    pub fn add_peer_as(
        &mut self,
        id: PeerId,
        addr: SocketAddr,
        name: String,
        role: PeerRole,
        now_ms: u64,
    ) -> bool {
        if self.peers.contains_key(&id) {
            return false;
        }
        self.reserved.remove(&id);
        // The address index too (ADR-8535): without it the server cannot find a resumed peer by
        // its address, so a handshake resend after a lost accept is admitted again as a new join.
        self.by_addr.insert(addr, id);
        self.peers.insert(id, Peer::new(id, addr, name, role, now_ms));
        true
    }

    /// Adds a peer; `None` when the session is full. Ids are issued from 1 upward and are not
    /// reused until all of `1..=u16::MAX` has been issued; after that the counter starts again
    /// at 1 (oldest first) and still never issues an id a live peer holds (ADR-5580). The
    /// session-level ids of `SessionServer` (spec 70 § 4) are separate and never reused.
    pub fn add_peer(
        &mut self,
        addr: SocketAddr,
        name: String,
        role: PeerRole,
        now_ms: u64,
    ) -> Option<PeerId> {
        if self.is_full() {
            return None;
        }
        let peer_id = self.issue_id()?;
        self.by_addr.insert(addr, peer_id);
        self.peers
            .insert(peer_id, Peer::new(peer_id, addr, name, role, now_ms));
        Some(peer_id)
    }

    /// Issues the next peer id without adding a peer or taking a slot: the id a pending
    /// one-round-trip resume of a held slot joins under once its client confirms
    /// ([`Self::add_peer_as`], ADR-8902). An id that is issued and never used is skipped until
    /// the counter comes round, like the id of a peer that left. `None` when every id is live or
    /// held.
    pub fn issue_id(&mut self) -> Option<PeerId> {
        // Ids go up from 1 and a live id is skipped. Past `u16::MAX` the counter starts again at
        // 1, oldest issued first, still skipping live ids (ADR-5580): refusing every later join
        // instead would let a client that joins and leaves 65,535 times shut the server to
        // everyone until it restarts. `is_full` caps live peers below `u16::MAX`, so a free id
        // always exists for a join and the walk ends.
        let mut id = self.next_id.max(1);
        let mut steps = 0u32;
        while self.peers.contains_key(&PeerId(u32::from(id)))
            || self.reserved.contains(&PeerId(u32::from(id)))
        {
            id = id.checked_add(1).unwrap_or(1);
            steps += 1;
            if steps > u32::from(u16::MAX) {
                return None;
            }
        }
        self.next_id = id.checked_add(1).unwrap_or(1);
        Some(PeerId(u32::from(id)))
    }

    /// Removes a peer.
    pub fn remove_peer(&mut self, id: PeerId) -> Option<Peer> {
        let peer = self.peers.remove(&id)?;
        if self.by_addr.get(&peer.addr) == Some(&id) {
            self.by_addr.remove(&peer.addr);
        }
        Some(peer)
    }

    /// Moves a peer to a new address (a validated rebind, ADR-8299) and keeps the address index
    /// in step. Change [`Peer::addr`] only through this: an edit through [`Self::peer_mut`] leaves
    /// the index pointing at the old address, and [`Self::peer_by_addr`] then misses the peer.
    /// `false` when no such peer exists.
    pub fn set_peer_addr(&mut self, id: PeerId, addr: SocketAddr) -> bool {
        let Some(peer) = self.peers.get_mut(&id) else {
            return false;
        };
        let old = std::mem::replace(&mut peer.addr, addr);
        if self.by_addr.get(&old) == Some(&id) {
            self.by_addr.remove(&old);
        }
        self.by_addr.insert(addr, id);
        true
    }

    /// A peer by id.
    pub fn peer(&self, id: PeerId) -> Option<&Peer> {
        self.peers.get(&id)
    }

    /// A peer by id, mutable.
    pub fn peer_mut(&mut self, id: PeerId) -> Option<&mut Peer> {
        self.peers.get_mut(&id)
    }

    /// The peer at `addr`, if any.
    ///
    /// One hash-map probe whatever the peer count (ADR-8299); the hit is checked against the
    /// peer's current address.
    pub fn peer_by_addr(&self, addr: SocketAddr) -> Option<PeerId> {
        let id = *self.by_addr.get(&addr)?;
        self.peers.get(&id).filter(|p| p.addr == addr).map(|p| p.id)
    }

    /// The peer at `addr`, counting the lookup in [`Self::addr_probes`]. The server's per-datagram
    /// path uses this one.
    pub(crate) fn peer_by_addr_counted(&mut self, addr: SocketAddr) -> Option<PeerId> {
        self.addr_probes += 1;
        self.peer_by_addr(addr)
    }

    /// Address-index probes the server's per-datagram lookups made since the session began: one
    /// per lookup, the same at 8 peers as at 128 (ADR-8299).
    pub fn addr_probes(&self) -> u64 {
        self.addr_probes
    }

    /// Every peer, in id order.
    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    /// Number of peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// True with no peers.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Records a packet from `id` at `now_ms`.
    pub fn touch(&mut self, id: PeerId, now_ms: u64) {
        if let Some(p) = self.peers.get_mut(&id) {
            p.last_seen_ms = p.last_seen_ms.max(now_ms);
        }
    }

    /// Peers silent for at least the policy timeout.
    pub fn timed_out(&self, now_ms: u64) -> Vec<PeerId> {
        let policy = self.policy.timeouts().sanitized();
        self.peers
            .values()
            .filter(|p| policy.expired(now_ms, p.joined_ms, p.last_seen_ms, p.started))
            .map(|p| p.id)
            .collect()
    }

    /// Health of one peer at `now_ms`.
    pub fn health(&self, id: PeerId, now_ms: u64) -> Option<PeerHealth> {
        let p = self.peers.get(&id)?;
        Some(PeerHealth {
            rtt_ms: p.link.rtt_ms,
            jitter_ms: p.link.jitter_ms,
            loss_fraction: p.link.loss_fraction,
            silent_ms: now_ms.saturating_sub(p.last_seen_ms),
            flagged: p.link.loss_fraction > self.policy.flag_loss_above
                || p.link.rtt_ms > self.policy.flag_rtt_above_ms,
        })
    }

    /// Bans `ip` until `expires_unix_s` (`0` = permanent). The ban holds for the address's block:
    /// the IPv4 address, or the whole IPv6 /64 ([`crate::addr_block`], ADR-3283). A second ban in
    /// the same block replaces the first, exact bans of addresses inside it included. The list
    /// keeps the newest [`MAX_BANS`]; a reason is cut to [`MAX_BAN_REASON_BYTES`].
    pub fn ban(&mut self, ip: IpAddr, mut reason: String, expires_unix_s: u64) {
        // One key per host: `10.0.0.1` and `::ffff:10.0.0.1` are the same peer (audit wave 5), and
        // one per IPv6 /64 (finding 28).
        let ip = ip.to_canonical();
        self.bans.retain(|b| !same_block(b.ip, ip));
        if self.bans.len() >= MAX_BANS {
            self.bans.remove(0);
        }
        if reason.len() > MAX_BAN_REASON_BYTES {
            let mut end = MAX_BAN_REASON_BYTES;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        self.bans.push(BanEntry {
            ip,
            reason,
            expires_unix_s,
            exact: false,
        });
    }

    /// Bans exactly `ip` (and no other address of its IPv6 /64) until `expires_unix_s` (`0` =
    /// permanent). A block ban covering `ip` stays: it is wider, not replaced. Use for an explicit
    /// `/128` of a `bans.ron` address entry so the game port refuses one address, not the block
    /// (ADR-4936).
    pub fn ban_exact(&mut self, ip: IpAddr, mut reason: String, expires_unix_s: u64) {
        let ip = ip.to_canonical();
        self.bans.retain(|b| !(b.exact && b.ip == ip));
        if self.bans.len() >= MAX_BANS {
            self.bans.remove(0);
        }
        if reason.len() > MAX_BAN_REASON_BYTES {
            let mut end = MAX_BAN_REASON_BYTES;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        self.bans.push(BanEntry {
            ip,
            reason,
            expires_unix_s,
            exact: true,
        });
    }

    /// Lifts the ban on `ip` (an exact ban of it) and on `ip`'s block (any address of a banned
    /// IPv6 /64 lifts a block ban); true when one existed.
    pub fn unban(&mut self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        let before = self.bans.len();
        self.bans.retain(|b| {
            if b.exact {
                b.ip != ip
            } else {
                !same_block(b.ip, ip)
            }
        });
        self.bans.len() != before
    }

    /// Whether `ip` is banned at `now_unix_s`: an exact entry matches `ip` alone, any other entry
    /// matches `ip`'s block (its IPv4 address or IPv6 /64).
    pub fn is_banned(&self, ip: IpAddr, now_unix_s: u64) -> bool {
        let ip = ip.to_canonical();
        self.bans.iter().any(|b| {
            let matched = if b.exact { b.ip == ip } else { same_block(b.ip, ip) };
            matched && (b.expires_unix_s == 0 || b.expires_unix_s > now_unix_s)
        })
    }

    /// Drops expired bans.
    pub fn prune_bans(&mut self, now_unix_s: u64) {
        self.bans
            .retain(|b| b.expires_unix_s == 0 || b.expires_unix_s > now_unix_s);
    }

    /// Current bans.
    pub fn bans(&self) -> &[BanEntry] {
        &self.bans
    }
}

/// Seconds since the Unix epoch from the system clock (0 if the clock is before 1970).
pub fn unix_now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit L5 (grok): the ban list and its reasons are bounded.
    #[test]
    fn the_ban_list_is_bounded() {
        let mut s = Session::new(4, HealthPolicy::default());
        for i in 0..(MAX_BANS as u32 + 3) {
            s.ban(IpAddr::from(i.to_be_bytes()), "é".repeat(400), 0);
        }
        assert_eq!(s.bans.len(), MAX_BANS);
        assert!(
            !s.is_banned(IpAddr::from(0u32.to_be_bytes()), 0),
            "the oldest left"
        );
        assert!(s.is_banned(IpAddr::from((MAX_BANS as u32 + 2).to_be_bytes()), 0));
        assert!(s.bans.iter().all(|b| b.reason.len() <= MAX_BAN_REASON_BYTES));
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn ids_skip_zero_and_live_ids_and_respect_capacity() {
        let mut s = Session::new(3, HealthPolicy::default());
        let a = s
            .add_peer(addr(1), "a".into(), PeerRole::Player, 0)
            .expect("slot");
        let b = s
            .add_peer(addr(2), "b".into(), PeerRole::Player, 0)
            .expect("slot");
        let c = s
            .add_peer(addr(3), "c".into(), PeerRole::Spectator, 0)
            .expect("slot");
        assert_eq!((a, b, c), (PeerId(1), PeerId(2), PeerId(3)));
        assert!(
            s.add_peer(addr(4), "d".into(), PeerRole::Player, 0).is_none(),
            "full"
        );
        s.remove_peer(b);
        let d = s
            .add_peer(addr(4), "d".into(), PeerRole::Player, 0)
            .expect("slot");
        assert_eq!(d, PeerId(4), "next id, not the freed one");
        assert_eq!(s.peer_by_addr(addr(4)), Some(d));
    }

    /// ADR-8535: a peer added under its held id (a resume) is found by its address, as a joined one
    /// is; before, the address index missed it and the server admitted a handshake resend from it
    /// (its accept lost) as a new join.
    #[test]
    fn a_resumed_peer_is_found_by_its_address() {
        let mut s = Session::new(3, HealthPolicy::default());
        let a = s
            .add_peer(addr(1), "a".into(), PeerRole::Player, 0)
            .expect("slot");
        s.remove_peer(a);
        s.reserve(a);
        assert_eq!(s.peer_by_addr(addr(1)), None);
        assert!(s.add_peer_as(a, addr(2), "a".into(), PeerRole::Player, 0));
        assert_eq!(s.peer_by_addr(addr(2)), Some(a), "the new address");
        assert_eq!(s.peer_by_addr(addr(1)), None, "not the old one");
        s.remove_peer(a);
        assert_eq!(
            s.peer_by_addr(addr(2)),
            None,
            "removed from the index with the peer"
        );
    }

    #[test]
    fn wraparound_does_not_reuse_an_id() {
        let mut s = Session::new(10, HealthPolicy::default());
        s.next_id = u16::MAX;
        let a = s
            .add_peer(addr(1), "a".into(), PeerRole::Player, 0)
            .expect("slot");
        assert_eq!(a, PeerId(u32::from(u16::MAX)));
        // ADR-5580: past `u16::MAX` the counter starts again at 1 instead of refusing every
        // later join. It never issues an id a live peer holds.
        let b = s
            .add_peer(addr(2), "b".into(), PeerRole::Player, 0)
            .expect("the counter starts again at 1 after u16::MAX");
        assert_eq!(b, PeerId(1));
        assert_ne!(b, a, "a live peer's id is never issued again");
        s.remove_peer(a);
        let c = s
            .add_peer(addr(3), "c".into(), PeerRole::Player, 0)
            .expect("slot");
        assert_eq!(c, PeerId(2), "oldest issued first, not the id just freed");
    }

    /// Before the wrap, a freed id is not issued again: ids go up from 1.
    #[test]
    fn a_freed_id_is_not_reissued_before_the_counter_wraps() {
        let mut s = Session::new(4, HealthPolicy::default());
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..1000u32 {
            let p = s
                .add_peer(addr(i as u16), "p".into(), PeerRole::Player, 0)
                .expect("slot");
            assert!(seen.insert(p), "{p:?} issued twice before the wrap");
            s.remove_peer(p);
        }
    }

    /// corr-net (ADR-5580): a client that joins and leaves 65,535 times must not shut the
    /// server to everyone. The old counter stopped at `u16::MAX` and refused every later join.
    #[test]
    fn corr_net_joins_keep_working_after_the_id_space_is_used_once() {
        let mut s = Session::new(4, HealthPolicy::default());
        let keep = s
            .add_peer(addr(9), "stays".into(), PeerRole::Player, 0)
            .expect("slot");
        for i in 0..70_000u32 {
            let p = s
                .add_peer(addr(1), "churn".into(), PeerRole::Player, 0)
                .unwrap_or_else(|| panic!("join {i} refused on an almost empty server"));
            assert_ne!(p, keep, "join {i} reused the live peer's id");
            assert_ne!(p, PeerId::SERVER);
            s.remove_peer(p);
        }
        assert!(s.peer(keep).is_some());
    }

    /// Review row 13: out-of-range timeouts are clamped where endpoints resolve the policy.
    #[test]
    fn timeout_policy_sanitized_clamps_every_value() {
        let p = |interrupt_ms, timeout_ms, start_grace_ms| TimeoutPolicy {
            interrupt_ms,
            timeout_ms,
            start_grace_ms,
        };
        assert_eq!(TimeoutPolicy::default().sanitized(), TimeoutPolicy::default());
        // Seconds written as milliseconds, and an interruption every tick.
        assert_eq!(p(0, 20, 0).sanitized(), p(100, 1_000, 0));
        // The interruption must come before the timeout.
        assert_eq!(p(30_000, 20_000, 0).sanitized(), p(19_999, 20_000, 0));
        assert_eq!(p(1_000, 1_000, 0).sanitized(), p(999, 1_000, 0));
        assert_eq!(p(1_500, 20_000, 3_600_000).sanitized(), p(1_500, 20_000, 600_000));
        assert_eq!(p(100, 1_000, 600_000).sanitized(), p(100, 1_000, 600_000));
        // A session silent past a 20 ms "timeout" is still kept for the 1 s floor.
        let mut s = Session::new(4, HealthPolicy::default().with_timeouts(p(0, 20, 0)));
        let a = s
            .add_peer(addr(1), "a".into(), PeerRole::Player, 0)
            .expect("slot");
        assert!(s.timed_out(999).is_empty());
        assert_eq!(s.timed_out(1_000), [a]);
    }

    #[test]
    fn silence_times_out_but_loss_only_flags() {
        let mut s = Session::new(
            4,
            HealthPolicy {
                timeout_ms: 10_000,
                start_grace_ms: 0,
                ..HealthPolicy::default()
            },
        );
        let a = s
            .add_peer(addr(1), "a".into(), PeerRole::Player, 0)
            .expect("slot");
        if let Some(p) = s.peer_mut(a) {
            p.link.loss_fraction = 0.5;
            p.link.rtt_ms = 400.0;
        }
        s.touch(a, 5_000);
        let h = s.health(a, 6_000).expect("peer");
        assert!(h.flagged);
        assert_eq!(h.silent_ms, 1_000);
        assert!(s.timed_out(14_999).is_empty(), "loss alone never removes");
        // Equality removes: silence of at least `timeout_ms` (10_000 here), not strictly longer.
        assert_eq!(s.timed_out(15_000), vec![a]);
    }

    #[test]
    fn bans_expire_and_lift() {
        let mut s = Session::new(4, HealthPolicy::default());
        let ip: IpAddr = [10, 0, 0, 9].into();
        s.ban(ip, "spam".into(), 100);
        assert!(s.is_banned(ip, 99));
        assert!(!s.is_banned(ip, 100));
        s.ban(ip, "again".into(), 0);
        assert_eq!(s.bans().len(), 1, "a second ban replaces the first");
        s.prune_bans(1_000_000);
        assert!(s.is_banned(ip, 1_000_000), "permanent");
        assert!(s.unban(ip));
        assert!(!s.is_banned(ip, 0));
    }

    /// Audit wave 5 (net-g2c): a transport ban covers both spellings of one IPv4 host, the plain
    /// address and its IPv4-mapped IPv6 form, whichever was banned and whichever reconnects.
    #[test]
    fn a_ban_covers_the_ipv4_mapped_form_of_the_same_host() {
        let v4: IpAddr = [10, 0, 0, 1].into();
        let mapped: IpAddr = std::net::Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped().into();
        for (banned, other) in [(v4, mapped), (mapped, v4)] {
            let mut s = Session::new(4, HealthPolicy::default());
            s.ban(banned, "spam".into(), 0);
            assert!(s.is_banned(other, 0), "{banned} bans {other}");
            s.ban(other, "again".into(), 0);
            assert_eq!(s.bans().len(), 1, "one host, one entry");
            assert!(s.unban(banned));
            assert!(!s.is_banned(other, 0));
        }
        let near: IpAddr = [10, 0, 0, 2].into();
        let mut s = Session::new(4, HealthPolicy::default());
        s.ban(mapped, "spam".into(), 0);
        assert!(!s.is_banned(near, 0));
    }

    /// An exact ban (ADR-4936) refuses one IPv6 address and no other address of its /64, expires
    /// like a block ban, and is lifted by `unban` without touching a neighbor's exact ban.
    #[test]
    fn an_exact_ban_holds_one_address_only() {
        let mut s = Session::new(4, HealthPolicy::default());
        let one: IpAddr = "2001:db8:1:2::dead:1".parse().expect("ip");
        let neighbor: IpAddr = "2001:db8:1:2::dead:2".parse().expect("ip");
        s.ban_exact(one, "bot".into(), 100);
        assert!(s.is_banned(one, 99));
        assert!(!s.is_banned(one, 100), "expired");
        s.ban_exact(one, "bot".into(), 0);
        assert!(!s.is_banned(neighbor, 0), "the /64 stays open");
        assert!(s.unban(one));
        assert!(!s.is_banned(one, 0));
        assert!(!s.unban(one), "already lifted");

        // A block ban replaces the exact entries of its block (it covers them); lifting the block
        // lifts them with it.
        let mut s = Session::new(4, HealthPolicy::default());
        s.ban_exact(one, "bot".into(), 0);
        s.ban(neighbor, "spam".into(), 0);
        assert_eq!(s.bans().len(), 1, "the block ban replaces the exact entry");
        assert!(s.is_banned(one, 0), "the block covers the whole /64");
        assert!(s.unban(neighbor));
        assert!(!s.is_banned(one, 0), "lifting the block lifts the /64");
        assert!(s.bans().is_empty());
    }
}
