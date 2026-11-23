// SPDX-License-Identifier: MPL-2.0
//! Bounded server state between `KeyInit` and the join: one entry per address, a short lifetime,
//! and a limit on how much curve work a second may cost. A session-token index finds the entry of
//! a client whose NAT gave it a new port after the `KeyReply` without scanning the table.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use super::SecureLink;
use crate::addr_block::same_block;

/// A handshake that shows no join within this long is forgotten, in milliseconds.
pub const PENDING_TTL_MS: u64 = 10_000;
/// Handshakes in flight from one IPv4 address or one IPv6 /64, the block one subscriber controls.
pub const MAX_PENDING_PER_IP: usize = 8;
/// Fewest entries a table holds, however small the server.
const MIN_CAPACITY: usize = 64;
/// Default of `ServerConfig::max_handshakes_per_s`.
pub const DEFAULT_HANDSHAKES_PER_S: u32 = 256;

/// One handshake waiting for its join token.
#[derive(Debug)]
pub struct PendingHandshake {
    /// The attempt's request nonce.
    pub client_nonce: u64,
    /// The first Noise message, so a resend gets the stored reply and costs no curve work.
    pub first_message: [u8; 32],
    /// The stored `KeyReply` datagram.
    pub reply: Vec<u8>,
    /// The session keys.
    pub link: SecureLink,
    /// When the cookie was minted (the password nonce is recomputed from it).
    pub minted_ms: u64,
    /// When this entry was created.
    pub created_ms: u64,
}

/// Why a handshake got no table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRefusal {
    /// The table is full of live entries.
    Full,
    /// This address block has too many handshakes in flight.
    PerIp,
}

/// Handshakes in flight, by source address.
#[derive(Debug)]
pub struct PendingTable {
    entries: HashMap<SocketAddr, PendingHandshake>,
    /// The address of an entry by its session token. Every value names an entry that holds that
    /// token, so the index never has more rows than `entries`.
    by_token: HashMap<u32, SocketAddr>,
    capacity: usize,
}

impl PendingTable {
    /// A table for a server with `max_peers` peers.
    pub fn new(max_peers: usize) -> Self {
        Self {
            entries: HashMap::new(),
            by_token: HashMap::new(),
            capacity: max_peers.saturating_mul(2).max(MIN_CAPACITY),
        }
    }

    /// Entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True with no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for `addr`.
    pub fn get(&self, addr: SocketAddr) -> Option<&PendingHandshake> {
        self.entries.get(&addr)
    }

    /// The entry for `addr`, mutably.
    pub fn get_mut(&mut self, addr: SocketAddr) -> Option<&mut PendingHandshake> {
        self.entries.get_mut(&addr)
    }

    /// Takes the entry for `addr`.
    pub fn remove(&mut self, addr: SocketAddr) -> Option<PendingHandshake> {
        let entry = self.entries.remove(&addr)?;
        self.unindex(entry.link.token(), addr);
        Some(entry)
    }

    /// Drops the index row of `token` if it names `addr` (another entry may hold the same token).
    fn unindex(&mut self, token: u32, addr: SocketAddr) {
        if self.by_token.get(&token) == Some(&addr) {
            self.by_token.remove(&token);
        }
    }

    /// Forgets entries past their lifetime.
    pub fn expire(&mut self, now_ms: u64) {
        self.entries
            .retain(|_, e| now_ms.saturating_sub(e.created_ms) < PENDING_TTL_MS);
        let entries = &self.entries;
        self.by_token
            .retain(|&token, &mut addr| entries.get(&addr).is_some_and(|e| e.link.token() == token));
    }

    /// The address of the entry with session `token` when it is another port of `from`'s IP
    /// (compared in canonical form): the client's NAT moved it after the `KeyReply`. One index
    /// lookup, no scan. The caller must authenticate a record under that entry's keys before it
    /// moves the entry with [`PendingTable::move_entry`].
    pub fn moved_from(&self, from: SocketAddr, token: u32) -> Option<SocketAddr> {
        let addr = *self.by_token.get(&token)?;
        (addr != from
            && addr.ip().to_canonical() == from.ip().to_canonical()
            && self.entries.get(&addr).is_some_and(|e| e.link.token() == token))
        .then_some(addr)
    }

    /// Moves the entry at `from` to `to`. Replaces nothing: false, and no change, when `from` holds
    /// no entry or `to` already holds one.
    pub fn move_entry(&mut self, from: SocketAddr, to: SocketAddr) -> bool {
        if self.entries.contains_key(&to) {
            return false;
        }
        let Some(entry) = self.remove(from) else {
            return false;
        };
        self.insert(to, entry);
        true
    }

    fn held_by_ip(&self, ip: IpAddr, except: SocketAddr) -> usize {
        self.entries
            .keys()
            .filter(|a| same_block(a.ip(), ip) && **a != except)
            .count()
    }

    /// Whether a new entry from `addr` would fit. Checked before any curve work.
    pub fn has_room(&mut self, addr: SocketAddr, now_ms: u64) -> Result<(), PendingRefusal> {
        if self.held_by_ip(addr.ip(), addr) >= MAX_PENDING_PER_IP {
            return Err(PendingRefusal::PerIp);
        }
        if self.entries.contains_key(&addr) || self.entries.len() < self.capacity {
            return Ok(());
        }
        self.expire(now_ms);
        if self.entries.len() < self.capacity {
            Ok(())
        } else {
            Err(PendingRefusal::Full)
        }
    }

    /// Stores an entry; a new attempt from one address replaces the old one.
    pub fn insert(&mut self, addr: SocketAddr, entry: PendingHandshake) {
        let token = entry.link.token();
        if let Some(old) = self.entries.insert(addr, entry) {
            self.unindex(old.link.token(), addr);
        }
        self.by_token.insert(token, addr);
    }
}

/// Caps the handshakes one second may run, so a flood of valid cookies cannot take the server's
/// time. A fixed one-second window, on the caller's clock.
#[derive(Debug, Clone)]
pub struct HandshakeGovernor {
    per_second: u32,
    second: u64,
    used: u32,
}

impl HandshakeGovernor {
    /// A governor for `per_second` handshakes (`0` = no limit).
    pub fn new(per_second: u32) -> Self {
        Self {
            per_second,
            second: 0,
            used: 0,
        }
    }

    /// True when another handshake may run at `now_ms`; counts it.
    pub fn try_admit(&mut self, now_ms: u64) -> bool {
        if self.per_second == 0 {
            return true;
        }
        let second = now_ms / 1000;
        if second != self.second {
            self.second = second;
            self.used = 0;
        }
        if self.used >= self.per_second {
            return false;
        }
        self.used += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{ClientHandshake, ServerIdentity, accept_handshake};

    fn entry(identity: &ServerIdentity, nonce: u64, created_ms: u64) -> PendingHandshake {
        entry_with_token(identity, nonce, created_ms, 1)
    }

    fn entry_with_token(
        identity: &ServerIdentity,
        nonce: u64,
        created_ms: u64,
        token: u32,
    ) -> PendingHandshake {
        let hs = ClientHandshake::start(&[1; 32], None).expect("start");
        let (link, reply) = accept_handshake(identity, &[1; 32], hs.message(), token).expect("accept");
        PendingHandshake {
            client_nonce: nonce,
            first_message: *hs.message(),
            reply: reply.to_vec(),
            link,
            minted_ms: created_ms,
            created_ms,
        }
    }

    fn addr(host: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, host], port))
    }

    #[test]
    fn one_entry_per_address_and_a_lifetime() {
        let id = ServerIdentity::generate().expect("random");
        let mut t = PendingTable::new(4);
        t.insert(addr(1, 1), entry(&id, 1, 0));
        t.insert(addr(1, 1), entry(&id, 2, 100));
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(addr(1, 1)).map(|e| e.client_nonce), Some(2));
        t.expire(100 + PENDING_TTL_MS - 1);
        assert_eq!(t.len(), 1, "inside the lifetime");
        t.expire(100 + PENDING_TTL_MS);
        assert!(t.is_empty());
    }

    /// Review row 20: a join from another port of the same IP finds its handshake by session token,
    /// only there, and the index follows every insert, move, removal and expiry.
    #[test]
    fn a_moved_port_finds_its_entry_by_token() {
        let id = ServerIdentity::generate().expect("random");
        let mut t = PendingTable::new(4);
        t.insert(addr(1, 1), entry_with_token(&id, 1, 0, 7));
        t.insert(addr(2, 1), entry_with_token(&id, 2, 0, 8));
        assert_eq!(t.moved_from(addr(1, 2), 7), Some(addr(1, 1)));
        assert_eq!(t.moved_from(addr(1, 1), 7), None, "the same address is no move");
        assert_eq!(t.moved_from(addr(3, 1), 7), None, "another IP");
        assert_eq!(t.moved_from(addr(1, 2), 8), None, "another IP's token");
        assert_eq!(t.moved_from(addr(1, 2), 9), None, "an unknown token");
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:2".parse().expect("mapped");
        assert_eq!(t.moved_from(mapped, 7), Some(addr(1, 1)), "canonical IP");
        assert!(!t.move_entry(addr(1, 1), addr(2, 1)), "never replaces an entry");
        assert!(!t.move_entry(addr(1, 9), addr(1, 8)), "nothing to move");
        assert!(t.move_entry(addr(1, 1), addr(1, 2)));
        assert!(t.get(addr(1, 1)).is_none());
        assert_eq!(t.get(addr(1, 2)).map(|e| e.client_nonce), Some(1));
        assert_eq!(
            t.moved_from(addr(1, 3), 7),
            Some(addr(1, 2)),
            "the index followed"
        );
        // A new attempt that replaces an entry drops the old token.
        t.insert(addr(1, 2), entry_with_token(&id, 3, 0, 9));
        assert_eq!(t.moved_from(addr(1, 3), 7), None);
        assert_eq!(t.moved_from(addr(1, 3), 9), Some(addr(1, 2)));
        assert!(t.remove(addr(1, 2)).is_some());
        assert_eq!(t.moved_from(addr(1, 3), 9), None);
        t.expire(PENDING_TTL_MS);
        assert!(t.is_empty());
        assert!(t.by_token.is_empty(), "the index is bounded by the table");
    }

    #[test]
    fn one_ip_cannot_hold_more_than_its_share() {
        let id = ServerIdentity::generate().expect("random");
        let mut t = PendingTable::new(4);
        for port in 0..MAX_PENDING_PER_IP as u16 {
            assert_eq!(t.has_room(addr(1, port), 0), Ok(()));
            t.insert(addr(1, port), entry(&id, 1, 0));
        }
        assert_eq!(t.has_room(addr(1, 999), 0), Err(PendingRefusal::PerIp));
        assert_eq!(t.has_room(addr(1, 0), 0), Ok(()), "replacing its own entry");
        assert_eq!(
            t.has_room(addr(2, 1), 0),
            Ok(()),
            "other addresses are unaffected"
        );
    }

    #[test]
    fn a_full_table_refuses_until_entries_expire() {
        let id = ServerIdentity::generate().expect("random");
        let mut t = PendingTable::new(0);
        for host in 0..=(MIN_CAPACITY / MAX_PENDING_PER_IP) as u8 {
            for port in 0..MAX_PENDING_PER_IP as u16 {
                if t.len() < MIN_CAPACITY {
                    t.insert(addr(host, port), entry(&id, 1, 0));
                }
            }
        }
        assert_eq!(t.len(), MIN_CAPACITY);
        assert_eq!(t.has_room(addr(200, 1), 1), Err(PendingRefusal::Full));
        assert_eq!(
            t.has_room(addr(200, 1), PENDING_TTL_MS),
            Ok(()),
            "expired entries leave"
        );
    }

    #[test]
    fn the_governor_caps_each_second_and_zero_means_no_limit() {
        let mut g = HandshakeGovernor::new(2);
        assert!(g.try_admit(1000));
        assert!(g.try_admit(1500));
        assert!(!g.try_admit(1999));
        assert!(g.try_admit(2000), "a new second");
        let mut open = HandshakeGovernor::new(0);
        assert!((0..10_000).all(|_| open.try_admit(0)));
    }

    #[test]
    fn one_ipv6_block_counts_as_one_source() {
        let ip = |text: &str| text.parse::<IpAddr>().expect("address");
        assert!(same_block(ip("2001:db8:1:2::1"), ip("2001:db8:1:2:ffff::9")));
        assert!(!same_block(ip("2001:db8:1:2::1"), ip("2001:db8:1:3::1")));
        assert!(same_block(ip("::ffff:10.0.0.1"), ip("10.0.0.1")));
        assert!(!same_block(ip("10.0.0.1"), ip("10.0.0.2")));
    }
}
